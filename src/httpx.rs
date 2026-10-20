//! DRF-shaped errors, pagination and request auth.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono_tz::Tz;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::auth::{self, AccessClaims};
use crate::state::AppState;

#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i32,
    pub is_staff: bool,
    pub is_superuser: bool,
    pub is_active: bool,
    pub username: String,
    pub tz: Tz,
    pub weight_unit: String,
    pub date_joined_days: i64,
}

pub fn json_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

pub fn ok_json(body: Value) -> Response {
    json_response(StatusCode::OK, body)
}

pub fn detail(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({"detail": message}))
}

pub fn not_found() -> Response {
    detail(StatusCode::NOT_FOUND, "Not found.")
}

pub fn not_authenticated() -> Response {
    detail(
        StatusCode::FORBIDDEN,
        "Authentication credentials were not provided.",
    )
}

pub fn permission_denied() -> Response {
    detail(
        StatusCode::FORBIDDEN,
        "You do not have permission to perform this action.",
    )
}

pub fn invalid_token() -> Response {
    detail(StatusCode::FORBIDDEN, "Invalid token.")
}

pub fn invalid_bearer() -> Response {
    json_response(
        StatusCode::FORBIDDEN,
        json!({
            "detail": "Given token not valid for any token type",
            "code": "token_not_valid",
            "messages": [{
                "token_class": "AccessToken",
                "token_type": "access",
                "message": "Token is invalid"
            }]
        }),
    )
}

pub fn method_not_allowed(method: &str) -> Response {
    detail(
        StatusCode::METHOD_NOT_ALLOWED,
        &format!("Method \"{method}\" not allowed."),
    )
}

/// Resolve the caller. A present but invalid credential is an error on every
/// route, including public ones (SimpleJWT raises before the permission check).
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<CurrentUser>, Response> {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let raw = raw.trim();
    if let Some(key) = raw.strip_prefix("Token ") {
        return match lookup_token(&state.pool, key.trim(), &state.config.time_zone).await {
            Ok(Some(user)) => Ok(Some(user)),
            Ok(None) => Err(invalid_token()),
            Err(resp) => Err(resp),
        };
    }
    if let Some(token) = raw.strip_prefix("Bearer ") {
        let claims = auth::read_access(&state.keys, token.trim()).map_err(|_| invalid_bearer())?;
        return match lookup_user_id(&state.pool, &claims, &state.config.time_zone).await {
            Ok(Some(user)) if user.is_active => Ok(Some(user)),
            Ok(_) => Err(invalid_bearer()),
            Err(resp) => Err(resp),
        };
    }
    Err(invalid_bearer())
}

pub async fn require_user(state: &AppState, headers: &HeaderMap) -> Result<CurrentUser, Response> {
    match authenticate(state, headers).await? {
        Some(user) => Ok(user),
        None => Err(not_authenticated()),
    }
}

async fn lookup_token(
    pool: &PgPool,
    key: &str,
    fallback: &Tz,
) -> Result<Option<CurrentUser>, Response> {
    let row = sqlx::query(
        r#"
        SELECT u.id, u.username, u.is_staff, u.is_superuser, u.is_active, u.date_joined,
               p.time_zone, p.weight_unit
        FROM authtoken_token t
        JOIN auth_user u ON u.id = t.user_id
        LEFT JOIN core_userprofile p ON p.user_id = u.id
        WHERE t.key = $1
        "#,
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(db_error)?;
    Ok(row.map(|row| user_from_row(&row, fallback)))
}

async fn lookup_user_id(
    pool: &PgPool,
    claims: &AccessClaims,
    fallback: &Tz,
) -> Result<Option<CurrentUser>, Response> {
    let Ok(id) = claims.sub.parse::<i32>() else {
        return Ok(None);
    };
    let row = sqlx::query(
        r#"
        SELECT u.id, u.username, u.is_staff, u.is_superuser, u.is_active, u.date_joined,
               p.time_zone, p.weight_unit
        FROM auth_user u
        LEFT JOIN core_userprofile p ON p.user_id = u.id
        WHERE u.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(db_error)?;
    Ok(row.map(|row| user_from_row(&row, fallback)))
}

fn user_from_row(row: &sqlx::postgres::PgRow, fallback: &Tz) -> CurrentUser {
    use sqlx::Row;
    let zone: Option<String> = row.try_get("time_zone").unwrap_or(None);
    let tz = zone
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<Tz>().ok())
        .unwrap_or(*fallback);
    let joined: chrono::DateTime<chrono::Utc> = row.get("date_joined");
    let days = (chrono::Utc::now() - joined).num_days();
    let weight_unit: Option<String> = row.try_get("weight_unit").unwrap_or(None);
    CurrentUser {
        id: row.get("id"),
        is_staff: row.get("is_staff"),
        is_superuser: row.get("is_superuser"),
        is_active: row.get("is_active"),
        username: row.get("username"),
        tz,
        weight_unit: weight_unit.unwrap_or_else(|| "kg".into()),
        date_joined_days: days,
    }
}

pub fn db_error(err: sqlx::Error) -> Response {
    tracing::error!(error = %err, "database");
    detail(StatusCode::INTERNAL_SERVER_ERROR, "Database error.")
}

pub struct Page {
    pub limit: i64,
    pub offset: i64,
}

pub fn page_from(pairs: &[(String, String)]) -> Page {
    let limit = pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "limit")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(|n| n.min(999))
        .unwrap_or(20);
    let offset = pairs
        .iter()
        .rev()
        .find(|(k, _)| k == "offset")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(0);
    Page { limit, offset }
}

pub fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .map(serde_urlencoded::from_str)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub fn paginated(
    uri: &Uri,
    headers: &HeaderMap,
    state: &AppState,
    count: i64,
    page: &Page,
    results: Vec<Value>,
) -> Value {
    // DRF LimitOffsetPagination returns an empty page when offset > count
    // (including Django's ApproximateCountManager reltuples of -1).
    let results = if page.offset > count {
        Vec::new()
    } else {
        results
    };
    let next = if page.offset + page.limit < count {
        Some(page_url(
            uri,
            headers,
            state,
            page.limit,
            page.offset + page.limit,
        ))
    } else {
        None
    };
    let previous = if page.offset > 0 {
        let prev = (page.offset - page.limit).max(0);
        Some(page_url(uri, headers, state, page.limit, prev))
    } else {
        None
    };
    json!({
        "count": count,
        "next": next,
        "previous": previous,
        "results": results,
    })
}

fn page_url(uri: &Uri, headers: &HeaderMap, state: &AppState, limit: i64, offset: i64) -> String {
    let mut pairs = query_pairs(uri);
    upsert(&mut pairs, "limit", &limit.to_string());
    if offset == 0 {
        pairs.retain(|(k, _)| k != "offset");
    } else {
        upsert(&mut pairs, "offset", &offset.to_string());
    }
    let qs = serde_urlencoded::to_string(&pairs).unwrap_or_default();
    let path = uri.path();
    let base = public_origin(headers, state);
    if qs.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{qs}")
    }
}

fn upsert(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(slot) = pairs.iter_mut().rev().find(|(k, _)| k == key) {
        slot.1 = value.to_string();
    } else {
        pairs.push((key.to_string(), value.to_string()));
    }
}

pub fn public_origin(headers: &HeaderMap, state: &AppState) -> String {
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string());
    let forwarded_host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string());
    match (forwarded_proto, forwarded_host) {
        (Some(proto), Some(host)) => format!("{proto}://{host}"),
        _ => state.config.site_url.clone(),
    }
}

pub fn qmap(pairs: &[(String, String)]) -> HashMap<String, String> {
    pairs.iter().cloned().collect()
}

pub fn one<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

pub fn parse_i32(value: &str) -> Option<i32> {
    value.parse().ok()
}

pub fn parse_i64_list(value: &str) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(part.parse().ok()?);
    }
    Some(out)
}

/// `ordering=-field,other`. Unknown names are dropped, matching DRF.
pub fn ordering<'a>(raw: Option<&str>, allowed: &[&'a str], default_sql: &'a str) -> String {
    let Some(raw) = raw else {
        return default_sql.to_string();
    };
    let mut parts = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (desc, name) = if let Some(rest) = item.strip_prefix('-') {
            (true, rest)
        } else {
            (false, item)
        };
        if allowed.contains(&name) {
            if desc {
                parts.push(format!("{name} DESC"));
            } else {
                parts.push(format!("{name} ASC"));
            }
        }
    }
    if parts.is_empty() {
        default_sql.to_string()
    } else {
        parts.join(", ")
    }
}

/// Prefix bare `ordering` columns. Defaults that already contain `.` are left alone,
/// so a second pass cannot turn `m.date` into `m.m.date`.
pub fn prefix_bare(order: &str, columns: &[(&str, &str)]) -> String {
    if order.contains('.') {
        return order.to_string();
    }
    order
        .split(", ")
        .map(|part| {
            let (name, dir) = part.split_once(' ').unwrap_or((part, "ASC"));
            let col = columns
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, sql)| *sql)
                .unwrap_or(name);
            format!("{col} {dir}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn head_or_json(method: &axum::http::Method, body: Value) -> Response {
    if *method == axum::http::Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        ok_json(body)
    }
}

pub async fn ping_db(State(state): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => ok_json(json!({"status": "ok"})),
        Err(err) => db_error(err),
    }
}
