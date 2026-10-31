//! Exercise catalogue reads: info, translations, images, muscles, equipment.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::routing::MethodRouter;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, QueryBuilder, Row};
use uuid::Uuid;

use crate::httpx::{self, head_or_json, page_from, paginated, public_origin, qmap, query_pairs};
use crate::state::AppState;
use crate::timefmt::fmt_dt;

use super::get_both;

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2/exerciseinfo", exerciseinfo));
    out.extend(get_both("/api/v2/exerciseinfo/{id}", exerciseinfo_one));
    out.extend(get_both("/api/v2/exercise", exercises));
    out.extend(get_both("/api/v2/exercise/{id}", exercise_one));
    out.extend(get_both("/api/v2/exercise-translation", translations));
    out.extend(get_both(
        "/api/v2/exercise-translation/{id}",
        translation_one,
    ));
    out.extend(get_both("/api/v2/equipment", equipment));
    out.extend(get_both("/api/v2/equipment/{id}", equipment_one));
    out.extend(get_both("/api/v2/exercisecategory", categories));
    out.extend(get_both("/api/v2/exercisecategory/{id}", category_one));
    out.extend(get_both("/api/v2/muscle", muscles));
    out.extend(get_both("/api/v2/muscle/{id}", muscle_one));
    out.extend(get_both("/api/v2/exerciseimage", images));
    out.extend(get_both("/api/v2/exerciseimage/{id}", image_one));
    out.extend(get_both("/api/v2/video", videos));
    out.extend(get_both("/api/v2/video/{id}", video_one));
    out.extend(get_both("/api/v2/exercisecomment", comments));
    out.extend(get_both("/api/v2/exercisecomment/{id}", comment_one));
    out.extend(get_both("/api/v2/exercisealias", aliases));
    out.extend(get_both("/api/v2/exercisealias/{id}", alias_one));
    out.extend(get_both("/api/v2/deletion-log", deletions));
    out
}

async fn exerciseinfo(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let (where_sql, _binds, order_sql) = exercise_filter(&map, true).await;
    let _english = english_id(&state.pool).await.unwrap_or(0);
    let count_sql = format!("SELECT COUNT(*) FROM exercises_exercise e WHERE {where_sql}");
    let count = match sqlx::query(sqlx::AssertSqlSafe(count_sql.clone())).fetch_one(&state.pool).await {
        Ok(row) => row.get::<i64, _>(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = if map.contains_key("name__search") && !map.contains_key("ordering") {
        order_sql
    } else {
        httpx::ordering(
            map.get("ordering").map(|s| s.as_str()),
            &["id", "uuid", "created", "last_update", "category_id"],
            "e.id ASC",
        )
    };
    let data_sql = format!(
        "SELECT e.id FROM exercises_exercise e WHERE {where_sql} ORDER BY {order} LIMIT {} OFFSET {}",
        page.limit, page.offset
    );
    let rows = match sqlx::query(sqlx::AssertSqlSafe(data_sql.clone())).fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let mut results = Vec::new();
    for row in rows {
        let id: i32 = row.get("id");
        if let Some(body) =
            exercise_info_json(&state.pool, id, &origin, tz, &state.config.media_root).await
        {
            results.push(body);
        }
    }
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn exerciseinfo_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let origin = public_origin(&headers, &state);
    match exercise_info_json(&state.pool, id, &origin, tz, &state.config.media_root).await {
        Some(body) => head_or_json(&method, body),
        None => httpx::not_found(),
    }
}

async fn exercises(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT COUNT(*) FROM exercises_exercise e WHERE EXISTS (SELECT 1 FROM exercises_translation t WHERE t.exercise_id = e.id)",
    );
    push_m2m_filters(&mut count_q, &map);
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::ordering(
        map.get("ordering").map(|s| s.as_str()),
        &["id", "uuid", "created", "last_update"],
        "e.id ASC",
    );
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT e.id, e.uuid, e.created, e.last_update, e.category_id, e.license_author, e.variation_group FROM exercises_exercise e WHERE EXISTS (SELECT 1 FROM exercises_translation t WHERE t.exercise_id = e.id)",
    );
    push_m2m_filters(&mut data_q, &map);
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
    let mut results = Vec::new();
    for row in &rows {
        results.push(exercise_brief(&state.pool, row, tz).await);
    }
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn exercise_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    method: Method,
) -> Response {
    let Ok(id) = raw.parse::<i32>() else {
        return httpx::not_found();
    };
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let row = match sqlx::query(
        "SELECT e.id, e.uuid, e.created, e.last_update, e.category_id, e.license_author, e.variation_group FROM exercises_exercise e WHERE e.id = $1 AND EXISTS (SELECT 1 FROM exercises_translation t WHERE t.exercise_id = e.id)",
    )
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
    head_or_json(&method, exercise_brief(&state.pool, &row, tz).await)
}

fn push_m2m_filters(q: &mut QueryBuilder<sqlx::Postgres>, map: &HashMap<String, String>) {
    if let Some(id) = map.get("category").and_then(|v| v.parse::<i32>().ok()) {
        q.push(" AND e.category_id = ");
        q.push_bind(id);
    }
    m2m(q, map, "muscles", "exercises_exercise_muscles", "muscle_id");
    m2m(
        q,
        map,
        "muscles_secondary",
        "exercises_exercise_muscles_secondary",
        "muscle_id",
    );
    m2m(
        q,
        map,
        "equipment",
        "exercises_exercise_equipment",
        "equipment_id",
    );
}

fn m2m(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &HashMap<String, String>,
    key: &str,
    table: &str,
    col: &str,
) {
    if let Some(id) = map.get(key).and_then(|v| v.parse::<i32>().ok()) {
        q.push(format!(
            " AND EXISTS (SELECT 1 FROM {table} m WHERE m.exercise_id = e.id AND m.{col} = "
        ));
        q.push_bind(id);
        q.push(")");
    }
    let in_key = format!("{key}__in");
    if let Some(raw) = map.get(&in_key) {
        let ids: Vec<i32> = raw
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if !ids.is_empty() {
            q.push(format!(" AND EXISTS (SELECT 1 FROM {table} m WHERE m.exercise_id = e.id AND m.{col} = ANY("));
            q.push_bind(ids);
            q.push("))");
        }
    }
}

async fn exercise_filter(map: &HashMap<String, String>, _info: bool) -> (String, Vec<i32>, String) {
    // Language and text search are interpolated as bound parameters by the
    // caller through a dedicated query. Integer filters travel in `binds`.
    let mut binds = Vec::new();
    let mut parts = vec!["true".to_string()];
    if let Some(raw) = map.get("id__in") {
        let ids: Vec<i32> = raw
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if ids.is_empty() {
            parts.push("false".into());
        } else {
            let list = ids
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            parts.push(format!("e.id IN ({list})"));
        }
    }
    if let Some(id) = map.get("id").and_then(|v| v.parse::<i32>().ok()) {
        parts.push(format!("e.id = {id}"));
    }
    if let Some(id) = map.get("category").and_then(|v| v.parse::<i32>().ok()) {
        parts.push(format!("e.category_id = {id}"));
    }
    if let Some(raw) = map.get("category__in") {
        let ids: Vec<i32> = raw
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if !ids.is_empty() {
            parts.push(format!(
                "e.category_id IN ({})",
                ids.iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    if let Some(id) = map.get("muscles").and_then(|v| v.parse::<i32>().ok()) {
        parts.push(format!(
            "EXISTS (SELECT 1 FROM exercises_exercise_muscles m WHERE m.exercise_id = e.id AND m.muscle_id = {id})"
        ));
    }
    if let Some(id) = map.get("equipment").and_then(|v| v.parse::<i32>().ok()) {
        parts.push(format!(
            "EXISTS (SELECT 1 FROM exercises_exercise_equipment m WHERE m.exercise_id = e.id AND m.equipment_id = {id})"
        ));
    }
    let langs = map.get("language__code").cloned();
    let search = map.get("name__search").cloned().filter(|s| !s.is_empty());
    let exact = map.get("name__exact").cloned().filter(|s| !s.is_empty());
    if let Some(term) = search.clone().or(exact.clone()) {
        let escaped = ilike_escape(&term);
        let lang_sql = language_sql(langs.as_deref(), true);
        if search.is_some() {
            parts.push(format!(
                "EXISTS (SELECT 1 FROM exercises_translation t LEFT JOIN exercises_alias a ON a.translation_id = t.id WHERE t.exercise_id = e.id {lang_sql} AND (t.name % {lit} OR {lit} <% t.name OR a.alias ILIKE {contains} ESCAPE E'\\\\'))"
                ,
                lit = sql_lit(&term),
                contains = sql_lit(&format!("%{escaped}%")),
            ));
        } else {
            parts.push(format!(
                "EXISTS (SELECT 1 FROM exercises_translation t LEFT JOIN exercises_alias a ON a.translation_id = t.id WHERE t.exercise_id = e.id {lang_sql} AND (t.name ILIKE {contains} ESCAPE E'\\\\' OR a.alias ILIKE {contains} ESCAPE E'\\\\'))",
                contains = sql_lit(&format!("%{escaped}%")),
            ));
        }
    } else if let Some(codes) = langs {
        let lang_sql = language_sql(Some(&codes), false);
        if !lang_sql.is_empty() {
            parts.push(format!(
                "EXISTS (SELECT 1 FROM exercises_translation t WHERE t.exercise_id = e.id {lang_sql})"
            ));
        }
    }
    let order = if let Some(term) = search {
        let lang_sql = language_sql(map.get("language__code").map(|s| s.as_str()), true);
        format!(
            "(SELECT CASE WHEN lower(t.name) = lower({lit}) THEN 3 WHEN lower(t.name) LIKE lower({prefix}) THEN 2 ELSE word_similarity({lit}, t.name) END FROM exercises_translation t WHERE t.exercise_id = e.id {lang_sql} ORDER BY 1 DESC, similarity(t.name, {lit}) DESC LIMIT 1) DESC, (SELECT similarity(t.name, {lit}) FROM exercises_translation t WHERE t.exercise_id = e.id {lang_sql} ORDER BY similarity(t.name, {lit}) DESC LIMIT 1) DESC",
            lit = sql_lit(&term),
            prefix = sql_lit(&format!("{}%", ilike_escape(&term))),
        )
    } else {
        "e.id ASC".into()
    };
    let _ = &mut binds;
    (parts.join(" AND "), binds, order)
}

fn language_sql(codes: Option<&str>, inside: bool) -> String {
    let Some(codes) = codes else {
        return String::new();
    };
    let list: Vec<String> = codes
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .map(|c| sql_lit(&c))
        .collect();
    if list.is_empty() {
        return String::new();
    }
    // Django's load_language splits on '-' and falls each unknown code back to English.
    let _ = inside;
    let values = list
        .iter()
        .map(|code| {
            let short = code.trim_matches('\'').split('-').next().unwrap_or(code);
            format!("({})", sql_lit(short))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "AND t.language_id IN (SELECT COALESCE((SELECT id FROM core_language WHERE short_name = v.code), (SELECT id FROM core_language WHERE short_name = 'en')) FROM (VALUES {values}) AS v(code))"
    )
}

fn sql_lit(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn ilike_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

async fn english_id(pool: &PgPool) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM core_language WHERE short_name = 'en' LIMIT 1")
        .fetch_one(pool)
        .await
}

async fn exercise_brief(pool: &PgPool, row: &PgRow, tz: chrono_tz::Tz) -> Value {
    let id: i32 = row.get("id");
    let created: DateTime<Utc> = row.get("created");
    let updated: DateTime<Utc> = row.get("last_update");
    let muscles = id_list(pool, "SELECT muscle_id FROM exercises_exercise_muscles em JOIN exercises_muscle m ON m.id = em.muscle_id WHERE em.exercise_id = $1 ORDER BY m.name, m.id", id).await;
    let secondary = id_list(pool, "SELECT muscle_id FROM exercises_exercise_muscles_secondary em JOIN exercises_muscle m ON m.id = em.muscle_id WHERE em.exercise_id = $1 ORDER BY m.name, m.id", id).await;
    let equipment = id_list(pool, "SELECT equipment_id FROM exercises_exercise_equipment ee JOIN exercises_equipment q ON q.id = ee.equipment_id WHERE ee.exercise_id = $1 ORDER BY q.name, q.id", id).await;
    json!({
        "id": id,
        "uuid": row.get::<Uuid, _>("uuid"),
        "created": fmt_dt(created, tz),
        "last_update": fmt_dt(updated, tz),
        "category": row.get::<i32, _>("category_id"),
        "muscles": muscles,
        "muscles_secondary": secondary,
        "equipment": equipment,
        "variation_group": row.get::<Option<Uuid>, _>("variation_group"),
        "license_author": row.get::<String, _>("license_author"),
    })
}

async fn id_list(pool: &PgPool, sql: &str, id: i32) -> Vec<i32> {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(id)
        .fetch_all(pool)
        .await
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
        .unwrap_or_default()
}

async fn exercise_info_json(
    pool: &PgPool,
    id: i32,
    origin: &str,
    tz: chrono_tz::Tz,
    media_root: &str,
) -> Option<Value> {
    let row = sqlx::query(
        r#"SELECT e.id, e.uuid, e.created, e.last_update, e.category_id, c.name AS category_name,
                  e.license_id, e.license_author, e.variation_group,
                  l.full_name AS license_full, l.short_name AS license_short, l.url AS license_url
           FROM exercises_exercise e
           JOIN exercises_exercisecategory c ON c.id = e.category_id
           JOIN core_license l ON l.id = e.license_id
           WHERE e.id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let created: DateTime<Utc> = row.get("created");
    let updated: DateTime<Utc> = row.get("last_update");
    let muscles = muscles_json(pool, origin, "exercises_exercise_muscles", id).await;
    let secondary = muscles_json(pool, origin, "exercises_exercise_muscles_secondary", id).await;
    let equipment = sqlx::query(
        "SELECT q.id, q.name FROM exercises_equipment q JOIN exercises_exercise_equipment ee ON ee.equipment_id = q.id WHERE ee.exercise_id = $1 ORDER BY q.name, q.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| json!({"id": r.get::<i32,_>("id"), "name": r.get::<String,_>("name")}))
    .collect::<Vec<_>>();
    let images = images_for(pool, id, origin, media_root).await;
    let videos = videos_for(pool, id, origin, false).await;
    let translations = translations_for(pool, id, tz).await;
    let global = last_update_global(pool, id, updated).await;
    let exercise_authors = authors(pool, "exercises_historicalexercise", "id", id).await;
    let mut total = exercise_authors.clone();
    for t in &translations {
        if let Some(id) = t.get("id").and_then(|v| v.as_i64()) {
            for name in authors(pool, "exercises_historicaltranslation", "id", id as i32).await {
                if !total.contains(&name) {
                    total.push(name);
                }
            }
        }
    }
    for image in &images {
        if let Some(id) = image.get("id").and_then(|v| v.as_i64()) {
            for name in authors(pool, "exercises_historicalexerciseimage", "id", id as i32).await {
                if !total.contains(&name) {
                    total.push(name);
                }
            }
        }
    }
    Some(json!({
        "id": id,
        "uuid": row.get::<Uuid, _>("uuid"),
        "created": fmt_dt(created, tz),
        "last_update": fmt_dt(updated, tz),
        "last_update_global": fmt_dt(global, tz),
        "category": {"id": row.get::<i32,_>("category_id"), "name": row.get::<String,_>("category_name")},
        "muscles": muscles,
        "muscles_secondary": secondary,
        "equipment": equipment,
        "license": {
            "id": row.get::<i32,_>("license_id"),
            "full_name": row.get::<String,_>("license_full"),
            "short_name": row.get::<String,_>("license_short"),
            "url": row.get::<String,_>("license_url"),
        },
        "license_author": row.get::<String,_>("license_author"),
        "images": images,
        "translations": translations,
        "variation_group": row.get::<Option<Uuid>, _>("variation_group"),
        "videos": videos,
        "author_history": exercise_authors,
        "total_authors_history": total,
    }))
}

async fn last_update_global(pool: &PgPool, id: i32, base: DateTime<Utc>) -> DateTime<Utc> {
    let row = sqlx::query(
        r#"SELECT GREATEST(
              $2,
              COALESCE((SELECT MAX(last_update) FROM exercises_exerciseimage WHERE exercise_id = $1), $2),
              COALESCE((SELECT MAX(last_update) FROM exercises_exercisevideo WHERE exercise_id = $1), $2),
              COALESCE((SELECT MAX(last_update) FROM exercises_translation WHERE exercise_id = $1), $2)
           ) AS ts"#,
    )
    .bind(id)
    .bind(base)
    .fetch_one(pool)
    .await;
    row.map(|r| r.get("ts")).unwrap_or(base)
}

async fn authors(pool: &PgPool, table: &str, col: &str, id: i32) -> Vec<String> {
    let sql = format!(
        "SELECT DISTINCT license_author FROM {table} WHERE {col} = $1 AND license_author <> '' ORDER BY license_author"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(id)
        .fetch_all(pool)
        .await
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
        .unwrap_or_default()
}

async fn muscles_json(pool: &PgPool, origin: &str, table: &str, id: i32) -> Vec<Value> {
    let sql = format!(
        "SELECT m.id, m.name, m.name_en, m.is_front FROM exercises_muscle m JOIN {table} em ON em.muscle_id = m.id WHERE em.exercise_id = $1 ORDER BY m.name, m.id"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| muscle_obj(r, origin))
        .collect()
}

fn muscle_obj(row: &PgRow, origin: &str) -> Value {
    let id: i32 = row.get("id");
    json!({
        "id": id,
        "name": row.get::<String,_>("name"),
        "name_en": row.get::<String,_>("name_en"),
        "is_front": row.get::<bool,_>("is_front"),
        "image_url_main": format!("{origin}/static/images/muscles/main/muscle-{id}.svg"),
        "image_url_secondary": format!("{origin}/static/images/muscles/secondary/muscle-{id}.svg"),
    })
}

async fn images_for(pool: &PgPool, exercise: i32, origin: &str, media_root: &str) -> Vec<Value> {
    let rows = sqlx::query(
        r#"SELECT i.id, i.uuid, i.exercise_id, e.uuid AS exercise_uuid, i.image, i.is_main, i.style,
                  i.license_id, i.license_title, i.license_object_url, i.license_author,
                  i.license_author_url, i.license_derivative_source_url, i.is_ai_generated
           FROM exercises_exerciseimage i
           JOIN exercises_exercise e ON e.id = i.exercise_id
           WHERE i.exercise_id = $1
           ORDER BY i.is_main DESC, i.id ASC"#,
    )
    .bind(exercise)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let id: i32 = row.get("id");
        let rel: String = row.get("image");
        let authors = authors(pool, "exercises_historicalexerciseimage", "id", id).await;
        out.push(json!({
            "id": id,
            "uuid": row.get::<Uuid,_>("uuid"),
            "exercise": row.get::<i32,_>("exercise_id"),
            "exercise_uuid": row.get::<Uuid,_>("exercise_uuid"),
            "image": format!("{origin}/media/{rel}"),
            "thumbnails": thumbs(origin, media_root, &rel),
            "is_main": row.get::<bool,_>("is_main"),
            "style": row.get::<String,_>("style"),
            "license": row.get::<i32,_>("license_id"),
            "license_title": row.get::<String,_>("license_title"),
            "license_object_url": row.get::<String,_>("license_object_url"),
            "license_author": row.get::<String,_>("license_author"),
            "license_author_url": row.get::<String,_>("license_author_url"),
            "license_derivative_source_url": row.get::<String,_>("license_derivative_source_url"),
            "author_history": authors,
            "is_ai_generated": row.get::<bool,_>("is_ai_generated"),
        }));
    }
    out
}

pub(super) fn thumbs(origin: &str, media_root: &str, rel: &str) -> Value {
    let path = std::path::Path::new(media_root).join(rel);
    if !path.is_file() {
        return Value::Null;
    }
    let lower = rel.to_ascii_lowercase();
    if !(lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".webp"))
    {
        return Value::Null;
    }
    let ext = rel.rsplit('.').next().unwrap_or("png");
    json!({
        "small": format!("{origin}/media/{rel}.200x200_q85.{ext}"),
        "medium": format!("{origin}/media/{rel}.400x400_q85.{ext}"),
    })
}

async fn videos_for(
    pool: &PgPool,
    exercise: i32,
    origin: &str,
    with_exercise_uuid: bool,
) -> Vec<Value> {
    let rows = sqlx::query(
        r#"SELECT v.id, v.uuid, v.exercise_id, e.uuid AS exercise_uuid, v.video, v.is_main, v.size,
                  v.duration, v.width, v.height, v.codec, v.codec_long, v.license_id, v.license_title,
                  v.license_object_url, v.license_author, v.license_author_url, v.license_derivative_source_url
           FROM exercises_exercisevideo v
           JOIN exercises_exercise e ON e.id = v.exercise_id
           WHERE v.exercise_id = $1
           ORDER BY v.is_main DESC, v.id ASC"#,
    )
    .bind(exercise)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let id: i32 = row.get("id");
        let rel: String = row.get("video");
        let authors = authors(pool, "exercises_historicalexercisevideo", "id", id).await;
        let mut obj = json!({
            "id": id,
            "uuid": row.get::<Uuid,_>("uuid"),
            "exercise": row.get::<i32,_>("exercise_id"),
            "video": format!("{origin}/media/{rel}"),
            "is_main": row.get::<bool,_>("is_main"),
            "size": row.get::<i32,_>("size"),
            "duration": crate::jsonutil::dec_str(row.get("duration"), 2),
            "width": row.get::<i32,_>("width"),
            "height": row.get::<i32,_>("height"),
            "codec": row.get::<String,_>("codec"),
            "codec_long": row.get::<String,_>("codec_long"),
            "license": row.get::<i32,_>("license_id"),
            "license_title": row.get::<String,_>("license_title"),
            "license_object_url": row.get::<String,_>("license_object_url"),
            "license_author": row.get::<String,_>("license_author"),
            "license_author_url": row.get::<String,_>("license_author_url"),
            "license_derivative_source_url": row.get::<String,_>("license_derivative_source_url"),
            "author_history": authors,
        });
        if with_exercise_uuid {
            obj.as_object_mut().unwrap().insert(
                "exercise_uuid".into(),
                Value::String(row.get::<Uuid, _>("exercise_uuid").to_string()),
            );
        }
        out.push(obj);
    }
    out
}

async fn translations_for(pool: &PgPool, exercise: i32, tz: chrono_tz::Tz) -> Vec<Value> {
    let rows = sqlx::query(
        r#"SELECT id, uuid, name, exercise_id, description, description_source, created, language_id,
                  license_id, license_title, license_object_url, license_author, license_author_url,
                  license_derivative_source_url
           FROM exercises_translation WHERE exercise_id = $1 ORDER BY name, id"#,
    )
    .bind(exercise)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let id: i32 = row.get("id");
        let created: DateTime<Utc> = row.get("created");
        let aliases = sqlx::query("SELECT id, uuid, alias FROM exercises_alias WHERE translation_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| json!({"id": r.get::<i32,_>("id"), "uuid": r.get::<Uuid,_>("uuid"), "alias": r.get::<String,_>("alias")}))
            .collect::<Vec<_>>();
        let notes = sqlx::query("SELECT id, uuid, translation_id, comment FROM exercises_exercisecomment WHERE translation_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| json!({"id": r.get::<i32,_>("id"), "uuid": r.get::<Uuid,_>("uuid"), "translation": r.get::<i32,_>("translation_id"), "comment": r.get::<String,_>("comment")}))
            .collect::<Vec<_>>();
        let authors = authors(pool, "exercises_historicaltranslation", "id", id).await;
        out.push(json!({
            "id": id,
            "uuid": row.get::<Uuid,_>("uuid"),
            "name": row.get::<String,_>("name"),
            "exercise": row.get::<i32,_>("exercise_id"),
            "description": row.get::<String,_>("description"),
            "description_source": row.get::<Option<String>,_>("description_source"),
            "created": fmt_dt(created, tz),
            "language": row.get::<i32,_>("language_id"),
            "aliases": aliases,
            "notes": notes,
            "license": row.get::<i32,_>("license_id"),
            "license_title": row.get::<String,_>("license_title"),
            "license_object_url": row.get::<String,_>("license_object_url"),
            "license_author": row.get::<String,_>("license_author"),
            "license_author_url": row.get::<String,_>("license_author_url"),
            "license_derivative_source_url": row.get::<String,_>("license_derivative_source_url"),
            "author_history": authors,
        }));
    }
    out
}

async fn translations(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("SELECT COUNT(*) FROM exercises_translation t WHERE true");
    translation_filters(&mut count_q, &map);
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::ordering(
        map.get("ordering").map(|s| s.as_str()),
        &[
            "id",
            "uuid",
            "name",
            "exercise",
            "description",
            "created",
            "language",
        ],
        "t.name ASC, t.id ASC",
    );
    let order = order
        .replace("exercise ", "t.exercise_id ")
        .replace("language ", "t.language_id ");
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT t.id, t.uuid, t.name, t.exercise_id, t.description, t.description_source, t.created, t.language_id, t.license_author FROM exercises_translation t WHERE true",
    );
    translation_filters(&mut data_q, &map);
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
    let results = rows
        .iter()
        .map(|row| {
            let created: DateTime<Utc> = row.get("created");
            json!({
                "id": row.get::<i32,_>("id"),
                "uuid": row.get::<Uuid,_>("uuid"),
                "name": row.get::<String,_>("name"),
                "exercise": row.get::<i32,_>("exercise_id"),
                "description": row.get::<String,_>("description"),
                "description_source": row.get::<Option<String>,_>("description_source"),
                "created": fmt_dt(created, tz),
                "language": row.get::<i32,_>("language_id"),
                "license_author": row.get::<String,_>("license_author"),
            })
        })
        .collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn translation_filters(q: &mut QueryBuilder<sqlx::Postgres>, map: &HashMap<String, String>) {
    // Django filterset_fields are exact matches on uuid, created, exercise,
    // description and name. language, q, search and icontains are ignored.
    if let Some(id) = map.get("exercise").and_then(|v| v.parse::<i32>().ok()) {
        q.push(" AND t.exercise_id = ");
        q.push_bind(id);
    }
    if let Some(name) = map.get("name") {
        q.push(" AND t.name = ");
        q.push_bind(name.clone());
    }
    if let Some(description) = map.get("description") {
        q.push(" AND t.description = ");
        q.push_bind(description.clone());
    }
    if let Some(uuid) = map.get("uuid").and_then(|v| Uuid::parse_str(v).ok()) {
        q.push(" AND t.uuid = ");
        q.push_bind(uuid);
    }
}

async fn translation_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let tz = auth
        .as_ref()
        .map(|u| u.tz)
        .unwrap_or(state.config.time_zone);
    let row = match sqlx::query(
        "SELECT id, uuid, name, exercise_id, description, description_source, created, language_id, license_author FROM exercises_translation WHERE id = $1",
    )
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
    let created: DateTime<Utc> = row.get("created");
    head_or_json(
        &method,
        json!({
            "id": row.get::<i32,_>("id"),
            "uuid": row.get::<Uuid,_>("uuid"),
            "name": row.get::<String,_>("name"),
            "exercise": row.get::<i32,_>("exercise_id"),
            "description": row.get::<String,_>("description"),
            "description_source": row.get::<Option<String>,_>("description_source"),
            "created": fmt_dt(created, tz),
            "language": row.get::<i32,_>("language_id"),
            "license_author": row.get::<String,_>("license_author"),
        }),
    )
}

async fn equipment(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    name_list(
        &state,
        &uri,
        &headers,
        &method,
        "exercises_equipment",
        "name ASC, id ASC",
    )
    .await
}
async fn equipment_one(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    name_one(&state, "exercises_equipment", id, &method).await
}
async fn categories(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    name_list(
        &state,
        &uri,
        &headers,
        &method,
        "exercises_exercisecategory",
        "name ASC, id ASC",
    )
    .await
}
async fn category_one(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    name_one(&state, "exercises_exercisecategory", id, &method).await
}

async fn name_list(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    table: &str,
    order: &str,
) -> Response {
    let _ = httpx::authenticate(state, headers).await;
    let pairs = query_pairs(uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, name FROM {table} ORDER BY {order} LIMIT $1 OFFSET $2"
    )))
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
        .map(|r| json!({"id": r.get::<i32,_>("id"), "name": r.get::<String,_>("name")}))
        .collect();
    head_or_json(
        method,
        paginated(uri, headers, state, count, &page, results),
    )
}

async fn name_one(state: &AppState, table: &str, id: i32, method: &Method) -> Response {
    let row = match sqlx::query(sqlx::AssertSqlSafe(format!("SELECT id, name FROM {table} WHERE id = $1")))
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
    head_or_json(
        method,
        json!({"id": row.get::<i32,_>("id"), "name": row.get::<String,_>("name")}),
    )
}

async fn muscles(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let _ = httpx::authenticate(&state, &headers).await;
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM exercises_muscle")
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query("SELECT id, name, name_en, is_front FROM exercises_muscle ORDER BY name, id LIMIT $1 OFFSET $2")
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&state.pool)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(|r| muscle_obj(r, &origin)).collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn muscle_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let origin = public_origin(&headers, &state);
    let row =
        match sqlx::query("SELECT id, name, name_en, is_front FROM exercises_muscle WHERE id = $1")
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
    head_or_json(&method, muscle_obj(&row, &origin))
}

async fn images(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let _ = httpx::authenticate(&state, &headers).await;
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM exercises_exerciseimage")
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query("SELECT exercise_id FROM exercises_exerciseimage GROUP BY exercise_id ORDER BY exercise_id LIMIT $1 OFFSET $2")
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&state.pool)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    // Flat image list, not grouped. Re-query the page of images directly.
    let _ = rows;
    let rows = match sqlx::query(
        r#"SELECT i.id FROM exercises_exerciseimage i ORDER BY i.is_main DESC, i.id ASC LIMIT $1 OFFSET $2"#,
    )
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let mut results = Vec::new();
    for row in rows {
        let id: i32 = row.get("id");
        if let Some(body) = image_json(&state.pool, id, &origin, &state.config.media_root).await {
            results.push(body);
        }
    }
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn image_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let origin = public_origin(&headers, &state);
    match image_json(&state.pool, id, &origin, &state.config.media_root).await {
        Some(body) => head_or_json(&method, body),
        None => httpx::not_found(),
    }
}

async fn image_json(pool: &PgPool, id: i32, origin: &str, media_root: &str) -> Option<Value> {
    let row = sqlx::query(
        r#"SELECT i.id, i.uuid, i.exercise_id, e.uuid AS exercise_uuid, i.image, i.is_main, i.style,
                  i.license_id, i.license_title, i.license_object_url, i.license_author,
                  i.license_author_url, i.license_derivative_source_url, i.is_ai_generated
           FROM exercises_exerciseimage i JOIN exercises_exercise e ON e.id = i.exercise_id WHERE i.id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let authors = authors(pool, "exercises_historicalexerciseimage", "id", id).await;
    let rel: String = row.get("image");
    Some(json!({
        "id": id,
        "uuid": row.get::<Uuid,_>("uuid"),
        "exercise": row.get::<i32,_>("exercise_id"),
        "exercise_uuid": row.get::<Uuid,_>("exercise_uuid"),
        "image": format!("{origin}/media/{rel}"),
        "thumbnails": thumbs(origin, media_root, &rel),
        "is_main": row.get::<bool,_>("is_main"),
        "style": row.get::<String,_>("style"),
        "license": row.get::<i32,_>("license_id"),
        "license_title": row.get::<String,_>("license_title"),
        "license_object_url": row.get::<String,_>("license_object_url"),
        "license_author": row.get::<String,_>("license_author"),
        "license_author_url": row.get::<String,_>("license_author_url"),
        "license_derivative_source_url": row.get::<String,_>("license_derivative_source_url"),
        "author_history": authors,
        "is_ai_generated": row.get::<bool,_>("is_ai_generated"),
    }))
}

async fn videos(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let _ = httpx::authenticate(&state, &headers).await;
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM exercises_exercisevideo")
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query("SELECT exercise_id FROM exercises_exercisevideo ORDER BY is_main DESC, id ASC LIMIT $1 OFFSET $2")
        .bind(page.limit)
        .bind(page.offset)
        .fetch_all(&state.pool)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let mut results = Vec::new();
    for row in rows {
        let exercise: i32 = row.get("exercise_id");
        let mut videos = videos_for(&state.pool, exercise, &origin, true).await;
        results.append(&mut videos);
    }
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn video_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let origin = public_origin(&headers, &state);
    let row = match sqlx::query("SELECT exercise_id FROM exercises_exercisevideo WHERE id = $1")
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
    let exercise: i32 = row.get("exercise_id");
    let videos = videos_for(&state.pool, exercise, &origin, true).await;
    let Some(body) = videos
        .into_iter()
        .find(|v| v.get("id").and_then(|n| n.as_i64()) == Some(id as i64))
    else {
        return httpx::not_found();
    };
    head_or_json(&method, body)
}

async fn comments(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    flat_uuid(
        &state,
        &uri,
        &headers,
        &method,
        "exercises_exercisecomment",
        "id, uuid, translation_id, comment",
        "id ASC",
        |row| {
            json!({
                "id": row.get::<i32,_>("id"),
                "uuid": row.get::<Uuid,_>("uuid"),
                "translation": row.get::<i32,_>("translation_id"),
                "comment": row.get::<String,_>("comment"),
            })
        },
    )
    .await
}
async fn comment_one(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    flat_one(&state, "exercises_exercisecomment", id, &method, |row| {
        json!({
            "id": row.get::<i32,_>("id"),
            "uuid": row.get::<Uuid,_>("uuid"),
            "translation": row.get::<i32,_>("translation_id"),
            "comment": row.get::<String,_>("comment"),
        })
    })
    .await
}
async fn aliases(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    flat_uuid(
        &state,
        &uri,
        &headers,
        &method,
        "exercises_alias",
        "id, uuid, translation_id, alias",
        "id ASC",
        |row| {
            json!({
                "id": row.get::<i32,_>("id"),
                "uuid": row.get::<Uuid,_>("uuid"),
                "translation": row.get::<i32,_>("translation_id"),
                "alias": row.get::<String,_>("alias"),
            })
        },
    )
    .await
}
async fn alias_one(State(state): State<AppState>, Path(id): Path<i32>, method: Method) -> Response {
    flat_one(&state, "exercises_alias", id, &method, |row| {
        json!({
            "id": row.get::<i32,_>("id"),
            "uuid": row.get::<Uuid,_>("uuid"),
            "translation": row.get::<i32,_>("translation_id"),
            "alias": row.get::<String,_>("alias"),
        })
    })
    .await
}

// The list helpers take the paging inputs one by one; bundling them would
// hide the SQL that each one builds.
#[allow(clippy::too_many_arguments)]
async fn flat_uuid<F>(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    table: &str,
    cols: &str,
    order: &str,
    map: F,
) -> Response
where
    F: Fn(&PgRow) -> Value,
{
    let _ = httpx::authenticate(state, headers).await;
    let pairs = query_pairs(uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {cols} FROM {table} ORDER BY {order} LIMIT $1 OFFSET $2"
    )))
    .bind(page.limit)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(map).collect();
    head_or_json(
        method,
        paginated(uri, headers, state, count, &page, results),
    )
}

async fn flat_one<F>(state: &AppState, table: &str, id: i32, method: &Method, map: F) -> Response
where
    F: Fn(&PgRow) -> Value,
{
    let cols = if table.ends_with("comment") {
        "id, uuid, translation_id, comment"
    } else {
        "id, uuid, translation_id, alias"
    };
    let row = match sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {cols} FROM {table} WHERE id = $1")))
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
    head_or_json(method, map(&row))
}

async fn deletions(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let _ = httpx::authenticate(&state, &headers).await;
    let tz = state.config.time_zone;
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM exercises_deletionlog")
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(
        "SELECT model_type, uuid, replaced_by, timestamp, comment FROM exercises_deletionlog ORDER BY id ASC LIMIT $1 OFFSET $2",
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
        .map(|row| {
            let ts: DateTime<Utc> = row.get("timestamp");
            json!({
                "model_type": row.get::<String,_>("model_type"),
                "uuid": row.get::<Uuid,_>("uuid"),
                "replaced_by": row.get::<Option<Uuid>,_>("replaced_by"),
                "timestamp": fmt_dt(ts, tz),
                "comment": row.get::<String,_>("comment"),
            })
        })
        .collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}
