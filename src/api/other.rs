//! Weight entries, measurements, trophies and gallery reads.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::routing::MethodRouter;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::{QueryBuilder, Row};
use uuid::Uuid;

use crate::httpx::{self, head_or_json, page_from, paginated, public_origin, qmap, query_pairs};
use crate::jsonutil::{dec_number, dec_str, json_f64};
use crate::state::AppState;
use crate::timefmt::{fmt_date, fmt_dt, fmt_time};

use super::get_both;

const LB_IN_KG: &str = "0.45359237";
const KG_IN_LBS: &str = "2.20462262";

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2/weightentry", weights));
    out.extend(get_both("/api/v2/weightentry/{id}", weight_one));
    out.extend(get_both("/api/v2/measurement", measurements));
    out.extend(get_both("/api/v2/measurement/{id}", measurement_one));
    out.extend(get_both("/api/v2/measurement-category", categories));
    out.extend(get_both("/api/v2/measurement-category/{id}", category_one));
    out.extend(get_both("/api/v2/trophy", trophies));
    out.extend(get_both("/api/v2/trophy/progress", trophy_progress));
    out.extend(get_both("/api/v2/trophy/{id}", trophy_one));
    out.extend(get_both("/api/v2/user-trophy", user_trophies));
    out.extend(get_both("/api/v2/user-trophy/{id}", user_trophy_one));
    out.extend(get_both("/api/v2/user-statistics", stats));
    out.extend(get_both("/api/v2/user-statistics/{id}", stats_one));
    out.extend(get_both("/api/v2/gallery", gallery));
    out.extend(get_both("/api/v2/gallery/{id}", gallery_one));
    out
}

async fn weights(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT COUNT(*) FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = ",
    );
    count_q.push_bind(user.id);
    count_q.push(" AND c.metric_type = 'body_weight' AND c.is_official = true");
    weight_filters(&mut count_q, &map);
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::prefix_bare(
        &httpx::ordering(
            map.get("ordering").map(|s| s.as_str()),
            &["id", "date", "weight"],
            "m.date ASC, m.id ASC",
        ),
        &[("id", "m.id"), ("date", "m.date"), ("weight", "m.value")],
    );
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT m.id, m.date, m.value, m.extra_data, c.unit AS category_unit, c.user_id FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = ",
    );
    data_q.push_bind(user.id);
    data_q.push(" AND c.metric_type = 'body_weight' AND c.is_official = true");
    weight_filters(&mut data_q, &map);
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
        .map(|row| weight_json(row, &user.weight_unit, user.tz))
        .collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn weight_filters(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
) {
    if let Some(date) = map.get("date").and_then(|v| parse_dt(v)) {
        q.push(" AND m.date = ");
        q.push_bind(date);
    }
    for (key, op) in [
        ("date__gt", ">"),
        ("date__gte", ">="),
        ("date__lt", "<"),
        ("date__lte", "<="),
    ] {
        if let Some(date) = map.get(key).and_then(|v| parse_dt(v)) {
            q.push(format!(" AND m.date {op} "));
            q.push_bind(date);
        }
    }
    if let Some(id) = map.get("id").and_then(|v| Uuid::parse_str(v).ok()) {
        q.push(" AND m.id = ");
        q.push_bind(id);
    }
    if let Some(w) = map.get("weight").and_then(|v| v.parse::<Decimal>().ok()) {
        q.push(" AND m.value = ");
        q.push_bind(w);
    }
}

fn parse_dt(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Some(ts.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| n.and_utc())
}

fn weight_json(row: &sqlx::postgres::PgRow, profile_unit: &str, tz: chrono_tz::Tz) -> Value {
    let date: DateTime<Utc> = row.get("date");
    let value: Decimal = row.get("value");
    let extra: Option<sqlx::types::Json<Value>> = row.try_get("extra_data").unwrap_or(None);
    let stored = extra
        .as_ref()
        .and_then(|j| j.0.get("unit"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| row.get::<&str, _>("category_unit"));
    let converted = convert_weight(value, stored, profile_unit);
    json!({
        "id": row.get::<Uuid,_>("id"),
        "date": fmt_dt(date, tz),
        "weight": dec_str(converted, 2),
        "user": row.get::<i32,_>("user_id"),
    })
}

fn convert_weight(value: Decimal, from: &str, to: &str) -> Decimal {
    if from == to {
        return value.round_dp(2);
    }
    let lb = Decimal::from_str_exact(LB_IN_KG).unwrap_or(Decimal::new(45359237, 8));
    let kg = Decimal::from_str_exact(KG_IN_LBS).unwrap_or(Decimal::new(220462262, 8));
    let as_kg = if from == "lb" { value * lb } else { value };
    let out = if to == "lb" { as_kg * kg } else { as_kg };
    // Django quantizes the converted value to 0.01 after a 4-place intermediate.
    out.round_dp(4).round_dp(2)
}

async fn weight_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT m.id, m.date, m.value, m.extra_data, c.unit AS category_unit, c.user_id FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = $1 AND c.metric_type = 'body_weight' AND c.is_official AND m.id = $2",
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
    head_or_json(&method, weight_json(&row, &user.weight_unit, user.tz))
}

async fn measurements(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query(
        "SELECT COUNT(*) FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = $1",
    )
    .bind(user.id)
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(
        "SELECT m.id, m.category_id, m.date, m.value, m.notes, m.source, m.external_id, m.extra_data FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = $1 ORDER BY m.date DESC, m.id DESC LIMIT $2 OFFSET $3",
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
    let results = rows
        .iter()
        .map(|row| measurement_json(row, user.tz))
        .collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn measurement_json(row: &sqlx::postgres::PgRow, tz: chrono_tz::Tz) -> Value {
    let date: DateTime<Utc> = row.get("date");
    let extra: Option<sqlx::types::Json<Value>> = row.try_get("extra_data").unwrap_or(None);
    json!({
        "id": row.get::<Uuid,_>("id"),
        "category": row.get::<Uuid,_>("category_id"),
        "date": fmt_dt(date, tz),
        "value": dec_number(row.get("value"), 2),
        "notes": row.get::<String,_>("notes"),
        "source": row.get::<String,_>("source"),
        "external_id": row.get::<Option<Uuid>,_>("external_id"),
        "extra_data": extra.map(|j| j.0).unwrap_or_else(|| json!({})),
    })
}

async fn measurement_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        "SELECT m.id, m.category_id, m.date, m.value, m.notes, m.source, m.external_id, m.extra_data FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE c.user_id = $1 AND m.id = $2",
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
    head_or_json(&method, measurement_json(&row, user.tz))
}

async fn categories(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 =
        match sqlx::query("SELECT COUNT(*) FROM measurements_category WHERE user_id = $1")
            .bind(user.id)
            .fetch_one(&state.pool)
            .await
        {
            Ok(row) => row.get(0),
            Err(err) => return httpx::db_error(err),
        };
    let rows = match sqlx::query(
        r#"SELECT id, name, unit, metric_type, chart_type, chart_config, parent_id, "order", is_official, dynamic_type, dynamic_params
           FROM measurements_category WHERE user_id = $1 ORDER BY "order", name, id LIMIT $2 OFFSET $3"#,
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
    let results = rows.iter().map(category_json).collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn category_json(row: &sqlx::postgres::PgRow) -> Value {
    let chart: Option<sqlx::types::Json<Value>> = row.try_get("chart_config").unwrap_or(None);
    let params: Option<sqlx::types::Json<Value>> = row.try_get("dynamic_params").unwrap_or(None);
    json!({
        "id": row.get::<Uuid,_>("id"),
        "name": row.get::<String,_>("name"),
        "unit": row.get::<String,_>("unit"),
        "metric_type": row.get::<String,_>("metric_type"),
        "chart_type": row.get::<Option<String>,_>("chart_type"),
        "chart_config": chart.map(|j| j.0).unwrap_or(Value::Null),
        "parent": row.get::<Option<Uuid>,_>("parent_id"),
        "order": row.get::<i32,_>("order"),
        "is_official": row.get::<bool,_>("is_official"),
        "dynamic_type": row.get::<String,_>("dynamic_type"),
        "dynamic_params": params.map(|j| j.0).unwrap_or(Value::Null),
    })
}

async fn category_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return httpx::not_found();
    };
    let row = match sqlx::query(
        r#"SELECT id, name, unit, metric_type, chart_type, chart_config, parent_id, "order", is_official, dynamic_type, dynamic_params
           FROM measurements_category WHERE user_id = $1 AND id = $2"#,
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
    head_or_json(&method, category_json(&row))
}

fn trophy_json(row: &sqlx::postgres::PgRow, origin: &str) -> Value {
    let uuid: Uuid = row.get("uuid");
    let kind: String = row.get("trophy_type");
    json!({
        "id": row.get::<i32,_>("id"),
        "uuid": uuid,
        "name": row.get::<String,_>("name"),
        "description": row.get::<String,_>("description"),
        "image": format!("{origin}/static/trophies/{kind}/{uuid}.png"),
        "trophy_type": kind,
        "is_hidden": row.get::<bool,_>("is_hidden"),
        "is_progressive": row.get::<bool,_>("is_progressive"),
        "is_repeatable": row.get::<bool,_>("is_repeatable"),
        "order": row.get::<i32,_>("order"),
    })
}

async fn trophies(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let (count, rows) = if auth.as_ref().is_some_and(|u| u.is_staff) {
        trophy_page(&state, "is_active = true", None, &page).await
    } else if let Some(user) = &auth {
        trophy_page(
            &state,
            "is_active = true AND (is_hidden = false OR id IN (SELECT trophy_id FROM trophies_usertrophy WHERE user_id = $1))",
            Some(user.id),
            &page,
        )
        .await
    } else {
        trophy_page(
            &state,
            "is_active = true AND is_hidden = false",
            None,
            &page,
        )
        .await
    };
    let (count, rows) = match (count, rows) {
        (Ok(c), Ok(r)) => (c, r),
        (Err(err), _) | (_, Err(err)) => return httpx::db_error(err),
    };
    let results = rows.iter().map(|row| trophy_json(row, &origin)).collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn trophy_page(
    state: &AppState,
    filter: &str,
    user: Option<i32>,
    page: &httpx::Page,
) -> (
    Result<i64, sqlx::Error>,
    Result<Vec<sqlx::postgres::PgRow>, sqlx::Error>,
) {
    let count_sql = format!("SELECT COUNT(*) FROM trophies_trophy WHERE {filter}");
    let data_sql = format!(
        "SELECT id, uuid, name, description, trophy_type, is_hidden, is_progressive, is_repeatable, \"order\" FROM trophies_trophy WHERE {filter} ORDER BY \"order\", name, id LIMIT {} OFFSET {}",
        page.limit, page.offset
    );
    let count = match user {
        Some(id) => sqlx::query(sqlx::AssertSqlSafe(count_sql.clone()))
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .map(|r| r.get(0)),
        None => sqlx::query(sqlx::AssertSqlSafe(count_sql.clone()))
            .fetch_one(&state.pool)
            .await
            .map(|r| r.get(0)),
    };
    let rows = match user {
        Some(id) => sqlx::query(sqlx::AssertSqlSafe(data_sql.clone())).bind(id).fetch_all(&state.pool).await,
        None => sqlx::query(sqlx::AssertSqlSafe(data_sql.clone())).fetch_all(&state.pool).await,
    };
    (count, rows)
}

async fn trophy_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let row = match sqlx::query(
        "SELECT id, uuid, name, description, trophy_type, is_hidden, is_progressive, is_repeatable, \"order\", is_active FROM trophies_trophy WHERE id = $1",
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
    if !row.get::<bool, _>("is_active") {
        return httpx::not_found();
    }
    if row.get::<bool, _>("is_hidden") && !auth.as_ref().is_some_and(|u| u.is_staff) {
        let earned = if let Some(user) = &auth {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM trophies_usertrophy WHERE user_id = $1 AND trophy_id = $2",
            )
            .bind(user.id)
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0)
        } else {
            0
        };
        if earned == 0 {
            return httpx::not_found();
        }
    }
    head_or_json(&method, trophy_json(&row, &origin))
}

async fn trophy_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let auth = match httpx::authenticate(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let Some(user) = auth else {
        return head_or_json(&method, json!([]));
    };
    let origin = public_origin(&headers, &state);
    let rows = match sqlx::query(
        r#"SELECT t.id, t.uuid, t.name, t.description, t.trophy_type, t.is_hidden, t.is_progressive, t.is_repeatable, t."order",
                  ut.earned_at, ut.progress
           FROM trophies_trophy t
           LEFT JOIN trophies_usertrophy ut ON ut.trophy_id = t.id AND ut.user_id = $1
           WHERE t.is_active AND ($2 OR t.is_hidden = false OR ut.id IS NOT NULL)
           ORDER BY t."order", t.name, t.id"#,
    )
    .bind(user.id)
    .bind(user.is_staff)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    // Live checker percentages are not ported. Earned rows keep the stored
    // progress; everything else is 0. current/target stay null.
    let body = rows
        .iter()
        .map(|row| {
            let earned_at: Option<DateTime<Utc>> = row.get("earned_at");
            let progress: Option<f64> = row.try_get("progress").unwrap_or(None);
            json!({
                "trophy": trophy_json(row, &origin),
                "is_earned": earned_at.is_some(),
                "earned_at": earned_at.map(|t| fmt_dt(t, user.tz)),
                "progress": json_f64(if earned_at.is_some() { progress.unwrap_or(100.0) } else { 0.0 }),
                "current_value": Value::Null,
                "target_value": Value::Null,
                "progress_display": Value::Null,
            })
        })
        .collect::<Vec<_>>();
    head_or_json(&method, Value::Array(body))
}

async fn user_trophies(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 =
        match sqlx::query("SELECT COUNT(*) FROM trophies_usertrophy WHERE user_id = $1")
            .bind(user.id)
            .fetch_one(&state.pool)
            .await
        {
            Ok(row) => row.get(0),
            Err(err) => return httpx::db_error(err),
        };
    let rows = match sqlx::query(
        r#"SELECT ut.id, ut.earned_at, ut.progress, ut.is_notified, ut.context_data,
                  t.id AS tid, t.uuid, t.name, t.description, t.trophy_type, t.is_hidden, t.is_progressive, t.is_repeatable, t."order"
           FROM trophies_usertrophy ut JOIN trophies_trophy t ON t.id = ut.trophy_id
           WHERE ut.user_id = $1 ORDER BY ut.earned_at DESC, ut.id DESC LIMIT $2 OFFSET $3"#,
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
    let results = rows
        .iter()
        .map(|row| {
            let earned: DateTime<Utc> = row.get("earned_at");
            let ctx: Option<sqlx::types::Json<Value>> = row.try_get("context_data").unwrap_or(None);
            let progress: f64 = row.try_get("progress").unwrap_or(0.0);
            json!({
                "id": row.get::<i32,_>("id"),
                "trophy": json!({
                    "id": row.get::<i32,_>("tid"),
                    "uuid": row.get::<Uuid,_>("uuid"),
                    "name": row.get::<String,_>("name"),
                    "description": row.get::<String,_>("description"),
                    "image": format!("{origin}/static/trophies/{}/{}.png", row.get::<String,_>("trophy_type"), row.get::<Uuid,_>("uuid")),
                    "trophy_type": row.get::<String,_>("trophy_type"),
                    "is_hidden": row.get::<bool,_>("is_hidden"),
                    "is_progressive": row.get::<bool,_>("is_progressive"),
                    "is_repeatable": row.get::<bool,_>("is_repeatable"),
                    "order": row.get::<i32,_>("order"),
                }),
                "earned_at": fmt_dt(earned, user.tz),
                "progress": json_f64(progress),
                "is_notified": row.get::<bool,_>("is_notified"),
                "context_data": ctx.map(|j| j.0).unwrap_or(Value::Null),
            })
        })
        .collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

async fn user_trophy_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let row = match sqlx::query(
        r#"SELECT ut.id, ut.earned_at, ut.progress, ut.is_notified, ut.context_data,
                  t.id AS tid, t.uuid, t.name, t.description, t.trophy_type, t.is_hidden, t.is_progressive, t.is_repeatable, t."order"
           FROM trophies_usertrophy ut JOIN trophies_trophy t ON t.id = ut.trophy_id
           WHERE ut.user_id = $1 AND ut.id = $2"#,
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
    let earned: DateTime<Utc> = row.get("earned_at");
    let ctx: Option<sqlx::types::Json<Value>> = row.try_get("context_data").unwrap_or(None);
    head_or_json(
        &method,
        json!({
            "id": row.get::<i32,_>("id"),
            "trophy": json!({
                "id": row.get::<i32,_>("tid"),
                "uuid": row.get::<Uuid,_>("uuid"),
                "name": row.get::<String,_>("name"),
                "description": row.get::<String,_>("description"),
                "image": format!("{origin}/static/trophies/{}/{}.png", row.get::<String,_>("trophy_type"), row.get::<Uuid,_>("uuid")),
                "trophy_type": row.get::<String,_>("trophy_type"),
                "is_hidden": row.get::<bool,_>("is_hidden"),
                "is_progressive": row.get::<bool,_>("is_progressive"),
                "is_repeatable": row.get::<bool,_>("is_repeatable"),
                "order": row.get::<i32,_>("order"),
            }),
            "earned_at": fmt_dt(earned, user.tz),
            "progress": json_f64(row.try_get("progress").unwrap_or(0.0)),
            "is_notified": row.get::<bool,_>("is_notified"),
            "context_data": ctx.map(|j| j.0).unwrap_or(Value::Null),
        }),
    )
}

async fn stats(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 =
        match sqlx::query("SELECT COUNT(*) FROM trophies_userstatistics WHERE user_id = $1")
            .bind(user.id)
            .fetch_one(&state.pool)
            .await
        {
            Ok(row) => row.get(0),
            Err(err) => return httpx::db_error(err),
        };
    let rows = match sqlx::query(
        r#"SELECT id, total_weight_lifted, total_workouts, current_streak, longest_streak, last_workout_date,
                  earliest_workout_time, latest_workout_time, weekend_workout_streak, last_complete_weekend_date,
                  worked_out_jan_1, last_updated
           FROM trophies_userstatistics WHERE user_id = $1 ORDER BY id LIMIT $2 OFFSET $3"#,
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
    let results = rows.iter().map(|row| stats_json(row, user.tz)).collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn stats_json(row: &sqlx::postgres::PgRow, tz: chrono_tz::Tz) -> Value {
    let last_date: Option<NaiveDate> = row.get("last_workout_date");
    let weekend: Option<NaiveDate> = row.get("last_complete_weekend_date");
    let early: Option<NaiveTime> = row.get("earliest_workout_time");
    let late: Option<NaiveTime> = row.get("latest_workout_time");
    let updated: DateTime<Utc> = row.get("last_updated");
    json!({
        "id": row.get::<i32,_>("id"),
        "total_weight_lifted": dec_str(row.get("total_weight_lifted"), 2),
        "total_workouts": row.get::<i32,_>("total_workouts"),
        "current_streak": row.get::<i32,_>("current_streak"),
        "longest_streak": row.get::<i32,_>("longest_streak"),
        "last_workout_date": last_date.map(fmt_date),
        "earliest_workout_time": early.map(fmt_time),
        "latest_workout_time": late.map(fmt_time),
        "weekend_workout_streak": row.get::<i32,_>("weekend_workout_streak"),
        "last_complete_weekend_date": weekend.map(fmt_date),
        "worked_out_jan_1": row.get::<bool,_>("worked_out_jan_1"),
        "last_updated": fmt_dt(updated, tz),
    })
}

async fn stats_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let row = match sqlx::query(
        r#"SELECT id, total_weight_lifted, total_workouts, current_streak, longest_streak, last_workout_date,
                  earliest_workout_time, latest_workout_time, weekend_workout_streak, last_complete_weekend_date,
                  worked_out_jan_1, last_updated
           FROM trophies_userstatistics WHERE user_id = $1 AND id = $2"#,
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
    head_or_json(&method, stats_json(&row, user.tz))
}

async fn gallery(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let pairs = query_pairs(&uri);
    let page = page_from(&pairs);
    let count: i64 = match sqlx::query("SELECT COUNT(*) FROM gallery_image WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let rows = match sqlx::query(
        "SELECT id, date, image, description, height, width FROM gallery_image WHERE user_id = $1 ORDER BY date DESC, id DESC LIMIT $2 OFFSET $3",
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
    let results = rows.iter().map(|row| gallery_json(row, &origin)).collect();
    head_or_json(
        &method,
        paginated(&uri, &headers, &state, count, &page, results),
    )
}

fn gallery_json(row: &sqlx::postgres::PgRow, origin: &str) -> Value {
    let date: NaiveDate = row.get("date");
    let rel: String = row.get("image");
    json!({
        "id": row.get::<i32,_>("id"),
        "date": fmt_date(date),
        "image": format!("{origin}/media/{rel}"),
        "description": row.get::<String,_>("description"),
        "height": row.get::<i32,_>("height"),
        "width": row.get::<i32,_>("width"),
    })
}

async fn gallery_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let origin = public_origin(&headers, &state);
    let row = match sqlx::query(
        "SELECT id, date, image, description, height, width FROM gallery_image WHERE user_id = $1 AND id = $2",
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
    head_or_json(&method, gallery_json(&row, &origin))
}

pub(super) async fn render_measurement(
    state: &AppState,
    headers: &HeaderMap,
    id: String,
) -> Response {
    measurement_one(State(state.clone()), headers.clone(), Path(id), Method::GET).await
}

pub(super) async fn render_weight(state: &AppState, headers: &HeaderMap, id: String) -> Response {
    weight_one(State(state.clone()), headers.clone(), Path(id), Method::GET).await
}
