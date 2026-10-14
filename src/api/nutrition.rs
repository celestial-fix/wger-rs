//! Ingredient, plan, meal and diary reads.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::routing::MethodRouter;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{QueryBuilder, Row};
use uuid::Uuid;

use crate::httpx::{self, head_or_json, page_from, paginated, public_origin, qmap, query_pairs};
use crate::jsonutil::{dec_str, f64_from_dec, json_f64, opt_dec_str};
use crate::state::AppState;
use crate::timefmt::{fmt_date, fmt_dt, fmt_time};

use super::get_both;

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2/ingredient", ingredients));
    out.extend(get_both("/api/v2/ingredient/:id", ingredient_one));
    out.extend(get_both("/api/v2/ingredientinfo", ingredientinfo));
    out.extend(get_both("/api/v2/ingredientinfo/:id", ingredientinfo_one));
    out.extend(get_both("/api/v2/ingredientweightunit", weight_units));
    out.extend(get_both("/api/v2/ingredientweightunit/:id", weight_unit_one));
    out.extend(get_both("/api/v2/nutritionplan", plans));
    out.extend(get_both("/api/v2/nutritionplan/:id", plan_one));
    out.extend(get_both("/api/v2/nutritionplan/:id/nutritional_values", plan_values));
    out.extend(get_both("/api/v2/nutritionplaninfo/:id", plan_info));
    out.extend(get_both("/api/v2/meal", meals));
    out.extend(get_both("/api/v2/meal/:id", meal_one));
    out.extend(get_both("/api/v2/mealitem", meal_items));
    out.extend(get_both("/api/v2/mealitem/:id", meal_item_one));
    out.extend(get_both("/api/v2/nutritiondiary", diary));
    out.extend(get_both("/api/v2/nutritiondiary/:id", diary_one));
    out
}

async fn ingredients(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    ingredient_page(&state, &uri, &headers, &method, false).await
}
async fn ingredientinfo(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    ingredient_page(&state, &uri, &headers, &method, true).await
}

async fn ingredient_page(state: &AppState, uri: &Uri, headers: &HeaderMap, method: &Method, info: bool) -> Response {
    let auth = match httpx::authenticate(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth.as_ref().map(|u| u.tz).unwrap_or(state.config.time_zone);
    let origin = public_origin(headers, state);
    let pairs = query_pairs(uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let filtered = ingredient_is_filtered(&map);
    let barcode_only = barcode_hit(&state.pool, &map).await;
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("SELECT i.id FROM nutrition_ingredient i WHERE true");
    push_ingredient_filters(&mut count_q, &map, barcode_only);
    let count_sql_prefix = if filtered {
        "SELECT COUNT(*) FROM ("
    } else {
        "SELECT COUNT(*) FROM ("
    };
    let cap = if filtered { " LIMIT 1000" } else { "" };
    let wrapped = format!("{count_sql_prefix}");
    let _ = wrapped;
    // Rebuild with a cap by selecting ids.
    let mut id_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("SELECT i.id FROM nutrition_ingredient i WHERE true");
    push_ingredient_filters(&mut id_q, &map, barcode_only);
    if map.get("name__search").is_none() && map.get("ordering").is_none() {
        // Unfiltered lists have no default ORDER BY.
    }
    let count: i64 = if filtered {
        let mut capped: QueryBuilder<sqlx::Postgres> =
            QueryBuilder::new("SELECT COUNT(*) FROM (SELECT i.id FROM nutrition_ingredient i WHERE true");
        push_ingredient_filters(&mut capped, &map, barcode_only);
        capped.push(" LIMIT 1000) c");
        match capped.build().fetch_one(&state.pool).await {
            Ok(row) => row.get(0),
            Err(err) => return httpx::db_error(err),
        }
    } else {
        // Match Django ApproximateCountManager: pg_class.reltuples (can be -1).
        match sqlx::query(
            "SELECT COALESCE((SELECT reltuples::bigint FROM pg_class WHERE oid = 'nutrition_ingredient'::regclass), -1)",
        )
        .fetch_one(&state.pool)
        .await
        {
            Ok(row) => row.get(0),
            Err(err) => return httpx::db_error(err),
        }
    };
    let _ = (count_q, count_sql_prefix, cap, id_q);
    let order = ingredient_order(&map);
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("SELECT i.id FROM nutrition_ingredient i WHERE true");
    push_ingredient_filters(&mut data_q, &map, barcode_only);
    if !order.is_empty() {
        data_q.push(" ORDER BY ");
        data_q.push(&order);
    }
    data_q.push(" LIMIT ");
    data_q.push_bind(page.limit);
    data_q.push(" OFFSET ");
    data_q.push_bind(page.offset);
    let rows = match data_q.build().fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let mut results = Vec::new();
    for row in rows {
        let id: i32 = row.get("id");
        if let Some(body) = ingredient_json(&state.pool, id, info, tz, &origin, &state.config.media_root).await {
            results.push(body);
        }
    }
    head_or_json(method, paginated(uri, headers, state, count, &page, results))
}

fn ingredient_is_filtered(map: &HashMap<String, String>) -> bool {
    map.keys().any(|k| {
        !matches!(k.as_str(), "limit" | "offset" | "ordering" | "format")
    })
}

async fn barcode_hit(pool: &sqlx::PgPool, map: &HashMap<String, String>) -> bool {
    let Some(term) = map.get("name__search") else {
        return false;
    };
    if !(term.chars().all(|c| c.is_ascii_digit()) && matches!(term.len(), 8 | 12 | 13 | 14)) {
        return false;
    }
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM nutrition_ingredient WHERE code = $1)")
        .bind(term)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
}

fn push_ingredient_filters(q: &mut QueryBuilder<sqlx::Postgres>, map: &HashMap<String, String>, barcode_only: bool) {
    if let Some(code) = map.get("code") {
        q.push(" AND i.code = ");
        q.push_bind(code.clone());
    }
    if let Some(name) = map.get("name") {
        q.push(" AND i.name = ");
        q.push_bind(name.clone());
    }
    if let Some(id) = map.get("id").and_then(|v| v.parse::<i32>().ok()) {
        q.push(" AND i.id = ");
        q.push_bind(id);
    }
    if let Some(raw) = map.get("id__in") {
        let ids: Vec<i32> = raw.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if ids.is_empty() {
            q.push(" AND false");
        } else {
            q.push(" AND i.id = ANY(");
            q.push_bind(ids);
            q.push(")");
        }
    }
    if let Some(codes) = map.get("language__code") {
        let list: Vec<String> = codes
            .split(',')
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        if !list.is_empty() {
            q.push(" AND i.language_id IN (SELECT id FROM core_language WHERE short_name = ANY(");
            q.push_bind(list);
            q.push("))");
        }
    }
    if let Some(term) = map.get("name__search").filter(|s| !s.is_empty()) {
        if barcode_only {
            // A numeric query that matches a stored barcode replaces the name search.
            q.push(" AND i.code = ");
            q.push_bind(term.clone());
        } else if !term.chars().any(|c| c.is_alphanumeric()) {
            q.push(" AND false");
        } else if term.chars().count() < 3 {
            q.push(" AND i.name ILIKE ");
            q.push_bind(term.clone());
        } else {
            push_name_search(q, term);
        }
    }
}

fn push_name_search(q: &mut QueryBuilder<sqlx::Postgres>, term: &str) {
    let single = !term.split_whitespace().nth(1).is_some();
    q.push(" AND (i.name % ");
    q.push_bind(term.to_string());
    if single {
        q.push(" OR i.name ILIKE ");
        q.push_bind(format!("%{}%", term.replace('%', "\\%").replace('_', "\\_")));
    }
    q.push(")");
}

fn ingredient_order(map: &HashMap<String, String>) -> String {
    if let Some(term) = map.get("name__search").filter(|s| s.chars().count() >= 3 && s.chars().any(|c| c.is_alphanumeric())) {
        if term.chars().all(|c| c.is_ascii_digit()) && matches!(term.len(), 8 | 12 | 13 | 14) {
            return String::new();
        }
        let lit = term.replace('\'', "''");
        return format!(
            "CASE WHEN i.name ILIKE '{lit}' THEN 3 WHEN i.name ILIKE '{lit}%' THEN 2 WHEN word_similarity('{lit}', i.name) = 1 THEN 1 ELSE 0 END DESC, similarity(i.name, '{lit}') DESC, i.name ASC, i.id ASC"
        );
    }
    if map.get("name__search").is_some() {
        return "i.name ASC, i.id ASC".into();
    }
    let allowed = ["id", "name", "code", "uuid", "remote_id", "last_update", "last_imported", "nutriscore"];
    if map.get("ordering").is_some() {
        return httpx::ordering(map.get("ordering").map(|s| s.as_str()), &allowed, "i.id ASC")
            .split(", ")
            .map(|part| {
                if part.contains('.') {
                    part.to_string()
                } else {
                    format!("i.{part}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    String::new()
}

async fn ingredient_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i32>, method: Method) -> Response {
    one_ingredient(&state, &headers, id, false, &method).await
}
async fn ingredientinfo_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<i32>, method: Method) -> Response {
    one_ingredient(&state, &headers, id, true, &method).await
}

async fn one_ingredient(state: &AppState, headers: &HeaderMap, id: i32, info: bool, method: &Method) -> Response {
    let auth = match httpx::authenticate(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth.as_ref().map(|u| u.tz).unwrap_or(state.config.time_zone);
    let origin = public_origin(headers, state);
    match ingredient_json(&state.pool, id, info, tz, &origin, &state.config.media_root).await {
        Some(body) => head_or_json(method, body),
        None => httpx::not_found(),
    }
}

async fn ingredient_json(
    pool: &sqlx::PgPool,
    id: i32,
    info: bool,
    tz: chrono_tz::Tz,
    origin: &str,
    media_root: &str,
) -> Option<Value> {
    let row = sqlx::query(
        r#"SELECT i.*, l.full_name AS license_full, l.short_name AS license_short, l.url AS license_url,
                  lang.short_name AS lang_short, lang.full_name AS lang_full, lang.full_name_en AS lang_full_en
           FROM nutrition_ingredient i
           LEFT JOIN core_license l ON l.id = i.license_id
           LEFT JOIN core_language lang ON lang.id = i.language_id
           WHERE i.id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let units = weight_units_for(pool, id).await;
    let created: DateTime<Utc> = row.get("created");
    let updated: DateTime<Utc> = row.get("last_update");
    let imported: Option<DateTime<Utc>> = row.get("last_imported");
    let mut body = json!({
        "id": id,
        "uuid": row.get::<Uuid,_>("uuid"),
        "remote_id": row.get::<Option<String>,_>("remote_id"),
        "source_name": row.get::<Option<String>,_>("source_name"),
        "source_url": row.get::<Option<String>,_>("source_url"),
        "code": row.get::<Option<String>,_>("code"),
        "name": row.get::<String,_>("name"),
        "common_name": row.get::<Option<String>,_>("common_name"),
        "brand": row.get::<Option<String>,_>("brand"),
        "created": fmt_dt(created, tz),
        "last_update": fmt_dt(updated, tz),
        "last_imported": imported.map(|t| fmt_dt(t, tz)),
        "energy": row.get::<i32,_>("energy"),
        "protein": dec_str(row.get("protein"), 3),
        "carbohydrates": dec_str(row.get("carbohydrates"), 3),
        "carbohydrates_sugar": opt_dec_str(row.get("carbohydrates_sugar"), 3),
        "fat": dec_str(row.get("fat"), 3),
        "fat_saturated": opt_dec_str(row.get("fat_saturated"), 3),
        "fiber": opt_dec_str(row.get("fiber"), 3),
        "sodium": opt_dec_str(row.get("sodium"), 3),
        "is_vegan": row.get::<Option<bool>,_>("is_vegan"),
        "is_vegetarian": row.get::<Option<bool>,_>("is_vegetarian"),
    });
    let obj = body.as_object_mut().unwrap();
    if info {
        obj.insert("nutriscore".into(), opt_str(row.get("nutriscore")));
        obj.insert("weight_units".into(), Value::Array(units));
        obj.insert(
            "language".into(),
            json!({
                "id": row.get::<i32,_>("language_id"),
                "short_name": row.get::<String,_>("lang_short"),
                "full_name": row.get::<String,_>("lang_full"),
                "full_name_en": row.get::<String,_>("lang_full_en"),
            }),
        );
        obj.insert(
            "license".into(),
            json!({
                "id": row.get::<i32,_>("license_id"),
                "full_name": row.get::<Option<String>,_>("license_full"),
                "short_name": row.get::<Option<String>,_>("license_short"),
                "url": row.get::<Option<String>,_>("license_url"),
            }),
        );
    } else {
        obj.insert("weight_units".into(), Value::Array(units));
        obj.insert("nutriscore".into(), opt_str(row.get("nutriscore")));
        obj.insert("license".into(), json!(row.get::<i32,_>("license_id")));
        obj.insert("language".into(), json!(row.get::<i32,_>("language_id")));
    }
    obj.insert("license_title".into(), Value::String(row.get("license_title")));
    obj.insert("license_object_url".into(), Value::String(row.get("license_object_url")));
    obj.insert("license_author".into(), Value::String(row.get("license_author")));
    obj.insert("license_author_url".into(), Value::String(row.get("license_author_url")));
    obj.insert(
        "license_derivative_source_url".into(),
        Value::String(row.get("license_derivative_source_url")),
    );
    if info {
        let image = ingredient_image(pool, id, origin, tz).await;
        obj.insert("image".into(), image.clone().unwrap_or(Value::Null));
        obj.insert("thumbnails".into(), ingredient_thumbs(pool, id, origin, media_root).await);
    }
    Some(body)
}

fn opt_str(value: Option<String>) -> Value {
    match value {
        Some(v) => Value::String(v),
        None => Value::Null,
    }
}

async fn weight_units_for(pool: &sqlx::PgPool, ingredient: i32) -> Vec<Value> {
    sqlx::query("SELECT id, uuid, ingredient_id, gram, name FROM nutrition_ingredientweightunit WHERE ingredient_id = $1 ORDER BY id")
        .bind(ingredient)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| json!({
            "id": r.get::<i32,_>("id"),
            "uuid": r.get::<Uuid,_>("uuid"),
            "ingredient": r.get::<i32,_>("ingredient_id"),
            "gram": r.get::<i32,_>("gram"),
            "name": r.get::<String,_>("name"),
        }))
        .collect()
}

async fn ingredient_image(pool: &sqlx::PgPool, ingredient: i32, origin: &str, tz: chrono_tz::Tz) -> Option<Value> {
    let table = "nutrition_image";
    let row = sqlx::query(&format!(
        r#"SELECT id, uuid, ingredient_id, image, created, last_update, size, width, height,
                  license_id, license_title, license_object_url, license_author, license_author_url,
                  license_derivative_source_url
           FROM {table} WHERE ingredient_id = $1"#
    ))
    .bind(ingredient)
    .fetch_optional(pool)
    .await
    .ok()??;
    let created: DateTime<Utc> = row.get("created");
    let updated: DateTime<Utc> = row.get("last_update");
    let rel: String = row.get("image");
    Some(json!({
        "id": row.get::<i32,_>("id"),
        "uuid": row.get::<Uuid,_>("uuid"),
        "ingredient_id": row.get::<Option<i32>,_>("ingredient_id"),
        "ingredient_uuid": Value::Null,
        "image": format!("{origin}/media/{rel}"),
        "created": fmt_dt(created, tz),
        "last_update": fmt_dt(updated, tz),
        "size": row.get::<i32,_>("size"),
        "width": row.get::<i32,_>("width"),
        "height": row.get::<i32,_>("height"),
        "license": row.get::<i32,_>("license_id"),
        "license_title": row.get::<String,_>("license_title"),
        "license_object_url": row.get::<String,_>("license_object_url"),
        "license_author": row.get::<String,_>("license_author"),
        "license_author_url": row.get::<String,_>("license_author_url"),
        "license_derivative_source_url": row.get::<String,_>("license_derivative_source_url"),
    }))
}

async fn ingredient_thumbs(pool: &sqlx::PgPool, ingredient: i32, origin: &str, media_root: &str) -> Value {
    let rel: Option<String> = sqlx::query_scalar("SELECT image FROM nutrition_image WHERE ingredient_id = $1")
        .bind(ingredient)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    let Some(rel) = rel else {
        return Value::Null;
    };
    super::exercise::thumbs(origin, media_root, &rel)
}

async fn weight_units(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    let _ = httpx::authenticate(&state, &headers).await;
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM nutrition_ingredientweightunit").fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(
        "SELECT id, uuid, ingredient_id, gram, name FROM nutrition_ingredientweightunit ORDER BY id LIMIT $1 OFFSET $2",
    )
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows
        .iter()
        .map(|r| json!({
            "id": r.get::<i32,_>("id"),
            "uuid": r.get::<Uuid,_>("uuid"),
            "ingredient": r.get::<i32,_>("ingredient_id"),
            "gram": r.get::<i32,_>("gram"),
            "name": r.get::<String,_>("name"),
        }))
        .collect();
    head_or_json(&method, paginated(&uri, &headers, &state, count, &page, results))
}

async fn weight_unit_one(State(state): State<AppState>, Path(id): Path<i32>, method: Method) -> Response {
    let row = match sqlx::query(
        "SELECT id, uuid, ingredient_id, gram, name FROM nutrition_ingredientweightunit WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(r) = row else {
        return httpx::not_found();
    };
    head_or_json(
        &method,
        json!({
            "id": r.get::<i32,_>("id"),
            "uuid": r.get::<Uuid,_>("uuid"),
            "ingredient": r.get::<i32,_>("ingredient_id"),
            "gram": r.get::<i32,_>("gram"),
            "name": r.get::<String,_>("name"),
        }),
    )
}

async fn plans(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM nutrition_nutritionplan WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(
        "SELECT id, creation_date, start, \"end\", description, only_logging, goal_energy, goal_protein, goal_carbohydrates, goal_fat, goal_fiber FROM nutrition_nutritionplan WHERE user_id = $1 ORDER BY start DESC NULLS LAST, id ASC LIMIT $2 OFFSET $3",
    )
    .bind(user.id)
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(plan_json).collect();
    head_or_json(&method, paginated(&uri, &headers, &state, count, &page, results))
}

fn plan_json(row: &PgRow) -> Value {
    let creation: NaiveDate = row.get("creation_date");
    let start: Option<NaiveDate> = row.get("start");
    let end: Option<NaiveDate> = row.get("end");
    json!({
        "id": row.get::<Uuid,_>("id"),
        "creation_date": fmt_date(creation),
        "start": start.map(fmt_date),
        "end": end.map(fmt_date),
        "description": row.get::<String,_>("description"),
        "only_logging": row.get::<bool,_>("only_logging"),
        "goal_energy": row.get::<Option<i32>,_>("goal_energy"),
        "goal_protein": row.get::<Option<i32>,_>("goal_protein"),
        "goal_carbohydrates": row.get::<Option<i32>,_>("goal_carbohydrates"),
        "goal_fat": row.get::<Option<i32>,_>("goal_fat"),
        "goal_fiber": row.get::<Option<i32>,_>("goal_fiber"),
    })
}

async fn plan_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT id, creation_date, start, \"end\", description, only_logging, goal_energy, goal_protein, goal_carbohydrates, goal_fat, goal_fiber FROM nutrition_nutritionplan WHERE user_id = $1 AND id = $2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    head_or_json(&method, plan_json(&row))
}

async fn plan_info(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT id, creation_date, start, \"end\", description, only_logging, goal_energy, goal_protein, goal_carbohydrates, goal_fat, goal_fiber FROM nutrition_nutritionplan WHERE user_id = $1 AND id = $2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    let mut body = plan_json(&row);
    let meals = meals_for(&state, id, &user, true).await;
    body.as_object_mut().unwrap().insert("meals".into(), Value::Array(meals));
    head_or_json(&method, body)
}

async fn plan_values(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let exists: Option<Uuid> = match sqlx::query_scalar("SELECT id FROM nutrition_nutritionplan WHERE user_id = $1 AND id = $2")
        .bind(user.id)
        .bind(id)
        .fetch_optional(&state.pool)
        .await
    {
        Ok(v) => v,
        Err(err) => return httpx::db_error(err),
    };
    if exists.is_none() {
        return httpx::not_found();
    }
    let values = nutritional_values(&state.pool, id).await;
    head_or_json(&method, values_json(&values))
}

struct Nuts {
    energy: Decimal,
    protein: Decimal,
    carbohydrates: Decimal,
    carbohydrates_sugar: Option<Decimal>,
    fat: Decimal,
    fat_saturated: Option<Decimal>,
    fiber: Option<Decimal>,
    sodium: Option<Decimal>,
}

fn add_opt(a: Option<Decimal>, b: Option<Decimal>) -> Option<Decimal> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

async fn nutritional_values(pool: &sqlx::PgPool, plan: Uuid) -> Nuts {
    let rows = sqlx::query(
        r#"SELECT mi.amount, wu.gram,
                  i.energy, i.protein, i.carbohydrates, i.carbohydrates_sugar, i.fat, i.fat_saturated, i.fiber, i.sodium
           FROM nutrition_mealitem mi
           JOIN nutrition_meal m ON m.id = mi.meal_id
           JOIN nutrition_ingredient i ON i.id = mi.ingredient_id
           LEFT JOIN nutrition_ingredientweightunit wu ON wu.id = mi.weight_unit_id
           WHERE m.plan_id = $1"#,
    )
    .bind(plan)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut total = Nuts {
        energy: Decimal::ZERO,
        protein: Decimal::ZERO,
        carbohydrates: Decimal::ZERO,
        carbohydrates_sugar: None,
        fat: Decimal::ZERO,
        fat_saturated: None,
        fiber: None,
        sodium: None,
    };
    for row in &rows {
        let amount: Decimal = row.get("amount");
        let gram: Option<i32> = row.get("gram");
        let weight = match gram {
            Some(g) => amount * Decimal::from(g),
            None => amount,
        };
        let scale = weight / Decimal::from(100);
        let energy: i32 = row.get("energy");
        total.energy += Decimal::from(energy) * scale;
        let protein: Decimal = row.get("protein");
        total.protein += protein * scale;
        let carbs: Decimal = row.get("carbohydrates");
        total.carbohydrates += carbs * scale;
        let fat: Decimal = row.get("fat");
        total.fat += fat * scale;
        let sugar: Option<Decimal> = row.get("carbohydrates_sugar");
        if sugar.is_some_and(|v| !v.is_zero()) {
            total.carbohydrates_sugar = add_opt(total.carbohydrates_sugar, sugar.map(|v| v * scale));
        }
        let sat: Option<Decimal> = row.get("fat_saturated");
        if sat.is_some_and(|v| !v.is_zero()) {
            total.fat_saturated = add_opt(total.fat_saturated, sat.map(|v| v * scale));
        }
        let fiber: Option<Decimal> = row.get("fiber");
        if fiber.is_some_and(|v| !v.is_zero()) {
            total.fiber = add_opt(total.fiber, fiber.map(|v| v * scale));
        }
        let sodium: Option<Decimal> = row.get("sodium");
        if sodium.is_some_and(|v| !v.is_zero()) {
            total.sodium = add_opt(total.sodium, sodium.map(|v| v * scale));
        }
    }
    total
}

fn values_json(v: &Nuts) -> Value {
    json!({
        "energy": json_f64(f64_from_dec(v.energy)),
        "protein": json_f64(f64_from_dec(v.protein)),
        "carbohydrates": json_f64(f64_from_dec(v.carbohydrates)),
        "carbohydrates_sugar": v.carbohydrates_sugar.map(|n| json_f64(f64_from_dec(n))),
        "fat": json_f64(f64_from_dec(v.fat)),
        "fat_saturated": v.fat_saturated.map(|n| json_f64(f64_from_dec(n))),
        "fiber": v.fiber.map(|n| json_f64(f64_from_dec(n))),
        "sodium": v.sodium.map(|n| json_f64(f64_from_dec(n))),
    })
}

async fn meals(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT COUNT(*) FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = ",
    );
    count_q.push_bind(user.id);
    if let Some(plan) = map.get("plan").and_then(|v| Uuid::parse_str(v).ok()) {
        count_q.push(" AND m.plan_id = ");
        count_q.push_bind(plan);
    }
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT m.id, m.plan_id, m.\"order\", m.time, m.name FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = ",
    );
    data_q.push_bind(user.id);
    if let Some(plan) = map.get("plan").and_then(|v| Uuid::parse_str(v).ok()) {
        data_q.push(" AND m.plan_id = ");
        data_q.push_bind(plan);
    }
    data_q.push(" ORDER BY m.time ASC NULLS LAST, m.id ASC LIMIT ");
    data_q.push_bind(page.limit);
    data_q.push(" OFFSET ");
    data_q.push_bind(page.offset);
    let rows = match data_q.build().fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(meal_json).collect();
    head_or_json(&method, paginated(&uri, &headers, &state, count, &page, results))
}

fn meal_json(row: &PgRow) -> Value {
    let time: Option<NaiveTime> = row.get("time");
    json!({
        "id": row.get::<Uuid,_>("id"),
        "plan": row.get::<Uuid,_>("plan_id"),
        "order": row.get::<i32,_>("order"),
        "time": time.map(fmt_time),
        "name": row.get::<Option<String>,_>("name"),
    })
}

async fn meal_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT m.id, m.plan_id, m.\"order\", m.time, m.name FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = $1 AND m.id = $2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    head_or_json(&method, meal_json(&row))
}

async fn meals_for(state: &AppState, plan: Uuid, user: &httpx::CurrentUser, with_items: bool) -> Vec<Value> {
    let rows = sqlx::query(
        "SELECT id, plan_id, \"order\", time, name FROM nutrition_meal WHERE plan_id = $1 ORDER BY time ASC NULLS LAST, id ASC",
    )
    .bind(plan)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let mut body = meal_json(row);
        if with_items {
            let meal: Uuid = row.get("id");
            let items = meal_items_info(&state.pool, meal, user.tz, &state.config).await;
            let values = meal_values(&state.pool, meal).await;
            if let Some(obj) = body.as_object_mut() {
                obj.insert("meal_items".into(), Value::Array(items));
                obj.insert("nutritional_values".into(), values_json(&values));
            }
        }
        out.push(body);
    }
    out
}

async fn meal_values(pool: &sqlx::PgPool, meal: Uuid) -> Nuts {
    let rows = sqlx::query(
        r#"SELECT mi.amount, wu.gram, i.energy, i.protein, i.carbohydrates, i.carbohydrates_sugar,
                  i.fat, i.fat_saturated, i.fiber, i.sodium
           FROM nutrition_mealitem mi
           JOIN nutrition_ingredient i ON i.id = mi.ingredient_id
           LEFT JOIN nutrition_ingredientweightunit wu ON wu.id = mi.weight_unit_id
           WHERE mi.meal_id = $1"#,
    )
    .bind(meal)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut total = Nuts {
        energy: Decimal::ZERO,
        protein: Decimal::ZERO,
        carbohydrates: Decimal::ZERO,
        carbohydrates_sugar: None,
        fat: Decimal::ZERO,
        fat_saturated: None,
        fiber: None,
        sodium: None,
    };
    for row in &rows {
        let amount: Decimal = row.get("amount");
        let gram: Option<i32> = row.get("gram");
        let weight = gram.map(|g| amount * Decimal::from(g)).unwrap_or(amount);
        let scale = weight / Decimal::from(100);
        total.energy += Decimal::from(row.get::<i32, _>("energy")) * scale;
        total.protein += row.get::<Decimal, _>("protein") * scale;
        total.carbohydrates += row.get::<Decimal, _>("carbohydrates") * scale;
        total.fat += row.get::<Decimal, _>("fat") * scale;
        if let Some(v) = row.get::<Option<Decimal>, _>("carbohydrates_sugar").filter(|v| !v.is_zero()) {
            total.carbohydrates_sugar = add_opt(total.carbohydrates_sugar, Some(v * scale));
        }
        if let Some(v) = row.get::<Option<Decimal>, _>("fat_saturated").filter(|v| !v.is_zero()) {
            total.fat_saturated = add_opt(total.fat_saturated, Some(v * scale));
        }
        if let Some(v) = row.get::<Option<Decimal>, _>("fiber").filter(|v| !v.is_zero()) {
            total.fiber = add_opt(total.fiber, Some(v * scale));
        }
        if let Some(v) = row.get::<Option<Decimal>, _>("sodium").filter(|v| !v.is_zero()) {
            total.sodium = add_opt(total.sodium, Some(v * scale));
        }
    }
    total
}

async fn meal_items(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT COUNT(*) FROM nutrition_mealitem mi JOIN nutrition_meal m ON m.id = mi.meal_id JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = ",
    );
    count_q.push_bind(user.id);
    if let Some(meal) = map.get("meal").and_then(|v| Uuid::parse_str(v).ok()) {
        count_q.push(" AND mi.meal_id = ");
        count_q.push_bind(meal);
    }
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT mi.id, mi.meal_id, mi.ingredient_id, mi.weight_unit_id, mi.\"order\", mi.amount FROM nutrition_mealitem mi JOIN nutrition_meal m ON m.id = mi.meal_id JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = ",
    );
    data_q.push_bind(user.id);
    if let Some(meal) = map.get("meal").and_then(|v| Uuid::parse_str(v).ok()) {
        data_q.push(" AND mi.meal_id = ");
        data_q.push_bind(meal);
    }
    data_q.push(" ORDER BY mi.\"order\" ASC, mi.id ASC LIMIT ");
    data_q.push_bind(page.limit);
    data_q.push(" OFFSET ");
    data_q.push_bind(page.offset);
    let rows = match data_q.build().fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(meal_item_json).collect();
    head_or_json(&method, paginated(&uri, &headers, &state, count, &page, results))
}

fn meal_item_json(row: &PgRow) -> Value {
    json!({
        "id": row.get::<Uuid,_>("id"),
        "meal": row.get::<Uuid,_>("meal_id"),
        "ingredient": row.get::<i32,_>("ingredient_id"),
        "weight_unit": row.get::<Option<i32>,_>("weight_unit_id"),
        "order": row.get::<i32,_>("order"),
        "amount": dec_str(row.get("amount"), 2),
    })
}

async fn meal_item_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT mi.id, mi.meal_id, mi.ingredient_id, mi.weight_unit_id, mi.\"order\", mi.amount FROM nutrition_mealitem mi JOIN nutrition_meal m ON m.id = mi.meal_id JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = $1 AND mi.id = $2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    head_or_json(&method, meal_item_json(&row))
}

async fn meal_items_info(pool: &sqlx::PgPool, meal: Uuid, tz: chrono_tz::Tz, config: &crate::config::Config) -> Vec<Value> {
    let rows = sqlx::query(
        "SELECT id, meal_id, ingredient_id, weight_unit_id, \"order\", amount FROM nutrition_mealitem WHERE meal_id = $1 ORDER BY \"order\", id",
    )
    .bind(meal)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let ingredient: i32 = row.get("ingredient_id");
        let unit: Option<i32> = row.get("weight_unit_id");
        let ingredient_obj = ingredient_json(pool, ingredient, true, tz, &config.site_url, &config.media_root)
            .await
            .unwrap_or(Value::Null);
        let unit_obj = if let Some(unit) = unit {
            sqlx::query("SELECT id, uuid, ingredient_id, gram, name FROM nutrition_ingredientweightunit WHERE id = $1")
                .bind(unit)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
                .map(|r| json!({
                    "id": r.get::<i32,_>("id"),
                    "uuid": r.get::<Uuid,_>("uuid"),
                    "ingredient": r.get::<i32,_>("ingredient_id"),
                    "gram": r.get::<i32,_>("gram"),
                    "name": r.get::<String,_>("name"),
                }))
        } else {
            None
        };
        out.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "meal": row.get::<Uuid,_>("meal_id"),
            "ingredient": ingredient,
            "ingredient_obj": ingredient_obj,
            "weight_unit": unit,
            "weight_unit_obj": unit_obj,
            "image": Value::Null,
            "order": row.get::<i32,_>("order"),
            "amount": dec_str(row.get("amount"), 2),
        }));
    }
    out
}

async fn diary(State(state): State<AppState>, uri: Uri, headers: HeaderMap, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT COUNT(*) FROM nutrition_logitem l JOIN nutrition_nutritionplan p ON p.id = l.plan_id WHERE p.user_id = ",
    );
    count_q.push_bind(user.id);
    push_diary_filters(&mut count_q, &map, user.tz);
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::prefix_bare(
        &httpx::ordering(
            map.get("ordering").map(|s| s.as_str()),
            &["id", "datetime", "amount", "plan", "ingredient"],
            "l.datetime DESC, l.id ASC",
        ),
        &[
            ("id", "l.id"),
            ("datetime", "l.datetime"),
            ("amount", "l.amount"),
            ("plan", "l.plan_id"),
            ("ingredient", "l.ingredient_id"),
        ],
    );
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT l.id, l.plan_id, l.meal_id, l.ingredient_id, l.weight_unit_id, l.datetime, l.amount FROM nutrition_logitem l JOIN nutrition_nutritionplan p ON p.id = l.plan_id WHERE p.user_id = ",
    );
    data_q.push_bind(user.id);
    push_diary_filters(&mut data_q, &map, user.tz);
    data_q.push(" ORDER BY ");
    data_q.push(order);
    data_q.push(" LIMIT ");
    data_q.push_bind(page.limit);
    data_q.push(" OFFSET ");
    data_q.push_bind(page.offset);
    let rows = match data_q.build().fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(|row| diary_json(row, user.tz)).collect();
    head_or_json(&method, paginated(&uri, &headers, &state, count, &page, results))
}

fn push_diary_filters(q: &mut QueryBuilder<sqlx::Postgres>, map: &HashMap<String, String>, tz: chrono_tz::Tz) {
    if let Some(plan) = map.get("plan").and_then(|v| Uuid::parse_str(v).ok()) {
        q.push(" AND l.plan_id = ");
        q.push_bind(plan);
    }
    if let Some(ingredient) = map.get("ingredient").and_then(|v| v.parse::<i32>().ok()) {
        q.push(" AND l.ingredient_id = ");
        q.push_bind(ingredient);
    }
    if let Some(date) = map.get("datetime__date").and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()) {
        q.push(" AND (l.datetime AT TIME ZONE ");
        q.push_bind(tz.name().to_string());
        q.push(")::date = ");
        q.push_bind(date);
    }
    for (suffix, op) in [("__gt", ">"), ("__gte", ">="), ("__lt", "<"), ("__lte", "<=")] {
        if let Some(raw) = map.get(&format!("datetime{suffix}")) {
            if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
                q.push(format!(" AND l.datetime {op} "));
                q.push_bind(ts.with_timezone(&Utc));
            }
        }
    }
}

fn diary_json(row: &PgRow, tz: chrono_tz::Tz) -> Value {
    let dt: DateTime<Utc> = row.get("datetime");
    json!({
        "id": row.get::<Uuid,_>("id"),
        "plan": row.get::<Uuid,_>("plan_id"),
        "meal": row.get::<Option<Uuid>,_>("meal_id"),
        "ingredient": row.get::<i32,_>("ingredient_id"),
        "weight_unit": row.get::<Option<i32>,_>("weight_unit_id"),
        "datetime": fmt_dt(dt, tz),
        "amount": dec_str(row.get("amount"), 2),
    })
}

async fn diary_one(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, method: Method) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT l.id, l.plan_id, l.meal_id, l.ingredient_id, l.weight_unit_id, l.datetime, l.amount FROM nutrition_logitem l JOIN nutrition_nutritionplan p ON p.id = l.plan_id WHERE p.user_id = $1 AND l.id = $2",
    )
    .bind(user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    head_or_json(&method, diary_json(&row, user.tz))
}
