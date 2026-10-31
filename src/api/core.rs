//! Version, profile, PowerSync, headless login and the small public catalogues.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Json, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::MethodRouter;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::auth::{self, needs_rehash};
use crate::httpx::{self, head_or_json, ok_json, page_from, paginated, qmap, query_pairs};
use crate::state::AppState;
use crate::timefmt::{fmt_date, fmt_dt};

use super::get_both;

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2", api_root));
    out.extend(get_both("/api/v2/version", version));
    out.extend(get_both("/api/v2/min-app-version", min_app));
    out.extend(get_both("/api/v2/min-server-version", min_server));
    out.extend(get_both("/api/v2/userprofile", userprofile));
    out.extend(get_both("/api/v2/powersync-token", powersync_token));
    out.extend(get_both("/api/v2/powersync-keys", powersync_keys));
    out.extend(get_both("/api/v2/language", languages));
    out.extend(get_both("/api/v2/license", licenses));
    out.extend(get_both("/api/v2/setting-repetitionunit", rep_units));
    out.extend(get_both("/api/v2/setting-weightunit", weight_units));
    out
}

async fn api_root(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let origin = httpx::public_origin(&headers, &state);
    let mut body = serde_json::Map::new();
    for name in API_ROOT {
        body.insert(
            (*name).to_string(),
            Value::String(format!("{origin}/api/v2/{name}/")),
        );
    }
    ok_json(Value::Object(body))
}

const API_ROOT: &[&str] = &[
    "routine",
    "templates",
    "public-templates",
    "workoutsession",
    "day",
    "slot",
    "slot-entry",
    "weight-config",
    "max-weight-config",
    "repetitions-config",
    "max-repetitions-config",
    "sets-config",
    "max-sets-config",
    "rest-config",
    "max-rest-config",
    "rir-config",
    "max-rir-config",
    "workoutlog",
    "language",
    "license",
    "setting-repetitionunit",
    "setting-weightunit",
    "exerciseinfo",
    "exercise-translation",
    "exercise",
    "equipment",
    "deletion-log",
    "exercisecategory",
    "video",
    "exerciseimage",
    "exercisecomment",
    "exercisealias",
    "muscle",
    "ingredient",
    "ingredientinfo",
    "ingredient-sync",
    "ingredientweightunit",
    "nutritionplan",
    "nutritionplaninfo",
    "nutritiondiary",
    "meal",
    "mealitem",
    "ingredient-image",
    "weightentry",
    "gallery",
    "measurement",
    "measurement-category",
    "trophy",
    "user-trophy",
    "user-statistics",
];

async fn version(State(state): State<AppState>, headers: HeaderMap, method: Method) -> Response {
    if let Err(resp) = httpx::authenticate(&state, &headers).await {
        return resp;
    }
    head_or_json(&method, Value::String(state.config.version.clone()))
}

async fn min_app(State(state): State<AppState>, headers: HeaderMap, method: Method) -> Response {
    if let Err(resp) = httpx::authenticate(&state, &headers).await {
        return resp;
    }
    head_or_json(&method, Value::String(state.config.min_app_version.clone()))
}

async fn min_server(State(state): State<AppState>, headers: HeaderMap, method: Method) -> Response {
    if let Err(resp) = httpx::authenticate(&state, &headers).await {
        return resp;
    }
    head_or_json(
        &method,
        Value::String(state.config.min_server_version.clone()),
    )
}

async fn powersync_keys(State(state): State<AppState>, method: Method) -> Response {
    head_or_json(&method, json!({"keys": [state.keys.public_jwk]}))
}

async fn powersync_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let token =
        match auth::mint_powersync(&state.keys, user.id, state.config.powersync_token_lifetime) {
            Ok(token) => token,
            Err(err) => {
                tracing::error!(error = %err, "powersync token");
                return httpx::detail(StatusCode::INTERNAL_SERVER_ERROR, "Could not sign token.");
            }
        };
    head_or_json(
        &method,
        json!({
            "token": token,
            "powersync_url": state.config.powersync_public_url(),
        }),
    )
}

async fn userprofile(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let row = match sqlx::query(
        r#"
        SELECT u.username, u.email, u.date_joined, u.is_superuser,
               p.is_temporary, p.workout_reminder_active, p.workout_reminder, p.workout_duration,
               p.last_workout_notification, p.age, p.birthdate, p.height, p.gender,
               p.sleep_hours, p.work_hours, p.work_intensity, p.sport_hours, p.sport_intensity,
               p.freetime_hours, p.freetime_intensity, p.calories, p.weight_unit,
               p.num_days_weight_reminder, p.time_zone, p.gym_id, p.notification_language_id,
               p.weight_rounding, p.repetitions_rounding,
               EXISTS (
                 SELECT 1 FROM account_emailaddress e
                 WHERE e.user_id = u.id AND e.email = u.email AND e.verified AND e.primary
               ) AS email_verified
        FROM auth_user u
        JOIN core_userprofile p ON p.user_id = u.id
        WHERE u.id = $1
        "#,
    )
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return httpx::not_found(),
        Err(err) => return httpx::db_error(err),
    };
    let joined: chrono::DateTime<Utc> = row.get("date_joined");
    let verified: bool = row.get("email_verified");
    let temporary: bool = row.get("is_temporary");
    let superuser: bool = row.get("is_superuser");
    let joined_date = joined.date_naive();
    let today = Utc::now().date_naive();
    let age_days = (today - joined_date).num_days();
    let trustworthy =
        superuser || (!temporary && age_days > state.config.min_account_age_days && verified);
    let birth: Option<chrono::NaiveDate> = row.get("birthdate");
    let last_note: Option<chrono::NaiveDate> = row.get("last_workout_notification");
    let body = json!({
        "username": row.get::<String, _>("username"),
        "email": row.get::<String, _>("email"),
        "email_verified": verified,
        "is_trustworthy": trustworthy,
        "date_joined": fmt_dt(joined, user.tz),
        "gym": row.get::<Option<i32>, _>("gym_id"),
        "weight_rounding": opt_dec(row.try_get("weight_rounding").ok().flatten(), 2),
        "repetitions_rounding": opt_dec(row.try_get("repetitions_rounding").ok().flatten(), 2),
        "is_temporary": temporary,
        "workout_reminder_active": row.get::<bool, _>("workout_reminder_active"),
        "workout_reminder": row.get::<i32, _>("workout_reminder"),
        "workout_duration": row.get::<i32, _>("workout_duration"),
        "last_workout_notification": last_note.map(fmt_date),
        "notification_language": row.get::<Option<i32>, _>("notification_language_id"),
        "age": row.get::<Option<i32>, _>("age"),
        "birthdate": birth.map(fmt_date),
        "height": row.get::<Option<i32>, _>("height"),
        "gender": row.get::<Option<String>, _>("gender"),
        "sleep_hours": row.get::<Option<i32>, _>("sleep_hours"),
        "work_hours": row.get::<Option<i32>, _>("work_hours"),
        "work_intensity": row.get::<Option<String>, _>("work_intensity"),
        "sport_hours": row.get::<Option<i32>, _>("sport_hours"),
        "sport_intensity": row.get::<Option<String>, _>("sport_intensity"),
        "freetime_hours": row.get::<Option<i32>, _>("freetime_hours"),
        "freetime_intensity": row.get::<Option<String>, _>("freetime_intensity"),
        "calories": row.get::<Option<i32>, _>("calories"),
        "weight_unit": row.get::<String, _>("weight_unit"),
        "num_days_weight_reminder": row.get::<i32, _>("num_days_weight_reminder"),
        "time_zone": row.get::<String, _>("time_zone"),
    });
    head_or_json(&method, body)
}

fn opt_dec(value: Option<rust_decimal::Decimal>, scale: u32) -> Value {
    crate::jsonutil::opt_dec_str(value, scale)
}

#[derive(Deserialize)]
pub(crate) struct LoginBody {
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
}

pub async fn login(State(state): State<AppState>, Json(body): Json<LoginBody>) -> Response {
    let Some(password) = body.password.filter(|p| !p.is_empty()) else {
        return allauth_error(400, "This field is required.", "required", Some("password"));
    };
    let username = body.username.filter(|s| !s.is_empty());
    let email = body.email.filter(|s| !s.is_empty());
    if username.is_some() == email.is_some() {
        return allauth_error(400, "Please select only one.", "select_only_one", None);
    }
    let row = if let Some(username) = &username {
        sqlx::query(
            "SELECT id, username, password, is_active, email FROM auth_user WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&state.pool)
        .await
    } else {
        sqlx::query(
            "SELECT id, username, password, is_active, email FROM auth_user WHERE lower(email) = lower($1)",
        )
        .bind(email.as_deref().unwrap_or(""))
        .fetch_optional(&state.pool)
        .await
    };
    let row = match row {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return mismatch(username.is_some());
    };
    let active: bool = row.get("is_active");
    let encoded: String = row.get("password");
    let ok = match auth::verify_password(&encoded, &password) {
        Ok(ok) => ok,
        Err(err) => {
            tracing::warn!(error = %err, "password verify");
            false
        }
    };
    if !ok || !active {
        return mismatch(username.is_some());
    }
    let id: i32 = row.get("id");
    let name: String = row.get("username");
    if needs_rehash(&encoded) {
        if let Ok(next) = auth::hash_argon2(&password) {
            let _ =
                sqlx::query("UPDATE auth_user SET password = $1, last_login = NOW() WHERE id = $2")
                    .bind(&next)
                    .bind(id)
                    .execute(&state.pool)
                    .await;
        }
    } else {
        let _ = sqlx::query("UPDATE auth_user SET last_login = NOW() WHERE id = $1")
            .bind(id)
            .execute(&state.pool)
            .await;
    }
    let sid = auth::new_sid();
    let access = match auth::mint_access(&state.keys, id, &sid, state.config.access_token_lifetime)
    {
        Ok(t) => t,
        Err(err) => return httpx::detail(StatusCode::INTERNAL_SERVER_ERROR, &err),
    };
    let refresh =
        match auth::mint_refresh(&state.keys, id, &sid, state.config.refresh_token_lifetime) {
            Ok(t) => t,
            Err(err) => return httpx::detail(StatusCode::INTERNAL_SERVER_ERROR, &err),
        };
    let primary_email: Option<String> = sqlx::query_scalar(
        "SELECT email FROM account_emailaddress WHERE user_id = $1 AND \"primary\" = true LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let mut user = serde_json::Map::new();
    user.insert("id".into(), json!(id));
    user.insert("display".into(), Value::String(name.clone()));
    if let Some(email) = primary_email.filter(|e| !e.is_empty()) {
        user.insert("email".into(), Value::String(email));
    }
    user.insert("has_usable_password".into(), json!(true));
    user.insert("username".into(), Value::String(name.clone()));
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut methods = json!({
        "method": "password",
        "at": at,
    });
    if let Some(obj) = methods.as_object_mut() {
        if username.is_some() {
            obj.insert("username".into(), Value::String(name));
        } else if let Some(email) = email {
            obj.insert("email".into(), Value::String(email));
        }
    }
    ok_json(json!({
        "status": 200,
        "data": {
            "user": user,
            "methods": [methods],
        },
        "meta": {
            "is_authenticated": true,
            "session_token": auth::new_sid(),
            "access_token": access,
            "refresh_token": refresh,
        }
    }))
}

fn mismatch(username: bool) -> Response {
    if username {
        allauth_error(
            400,
            "The username and/or password you specified are not correct.",
            "username_password_mismatch",
            Some("password"),
        )
    } else {
        allauth_error(
            400,
            "The email address and/or password you specified are not correct.",
            "email_password_mismatch",
            Some("password"),
        )
    }
}

fn allauth_error(status: u16, message: &str, code: &str, param: Option<&str>) -> Response {
    let mut err = json!({"message": message, "code": code});
    if let Some(param) = param {
        err.as_object_mut()
            .unwrap()
            .insert("param".into(), Value::String(param.into()));
    }
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    httpx::json_response(
        code,
        json!({
            "status": status,
            "errors": [err],
        }),
    )
}

#[derive(Deserialize)]
pub(crate) struct RefreshBody {
    refresh_token: Option<String>,
}

pub async fn refresh(
    State(state): State<AppState>,
    body: Result<Json<RefreshBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return httpx::json_response(StatusCode::BAD_REQUEST, json!({"status": 400})),
    };
    let Some(token) = body.refresh_token.filter(|s| !s.is_empty()) else {
        return allauth_error(
            400,
            "This field is required.",
            "required",
            Some("refresh_token"),
        );
    };
    let Ok(claims) = auth::read_refresh(&state.keys, &token) else {
        return httpx::json_response(StatusCode::BAD_REQUEST, json!({"status": 400}));
    };
    let Ok(id) = claims.sub.parse::<i32>() else {
        return httpx::json_response(StatusCode::BAD_REQUEST, json!({"status": 400}));
    };
    let active: Option<bool> = sqlx::query_scalar("SELECT is_active FROM auth_user WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    if active != Some(true) {
        return httpx::json_response(StatusCode::BAD_REQUEST, json!({"status": 400}));
    }
    let access = match auth::mint_access(
        &state.keys,
        id,
        &claims.sid,
        state.config.access_token_lifetime,
    ) {
        Ok(t) => t,
        Err(_) => return httpx::detail(StatusCode::INTERNAL_SERVER_ERROR, "Could not sign token."),
    };
    let refresh = match auth::mint_refresh(
        &state.keys,
        id,
        &claims.sid,
        state.config.refresh_token_lifetime,
    ) {
        Ok(t) => t,
        Err(_) => return httpx::detail(StatusCode::INTERNAL_SERVER_ERROR, "Could not sign token."),
    };
    ok_json(json!({
        "status": 200,
        "data": {
            "access_token": access,
            "refresh_token": refresh,
        }
    }))
}

async fn languages(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    simple_list(
        &state,
        &uri,
        &headers,
        &method,
        "SELECT id, short_name, full_name, full_name_en FROM core_language",
        "full_name ASC, id ASC",
        &["id", "short_name", "full_name", "full_name_en"],
        |row| {
            json!({
                "id": row.get::<i32, _>("id"),
                "short_name": row.get::<String, _>("short_name"),
                "full_name": row.get::<String, _>("full_name"),
                "full_name_en": row.get::<String, _>("full_name_en"),
            })
        },
    )
    .await
}

async fn licenses(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    simple_list(
        &state,
        &uri,
        &headers,
        &method,
        "SELECT id, full_name, short_name, url FROM core_license",
        "full_name ASC, id ASC",
        &["id", "full_name", "short_name", "url"],
        |row| {
            json!({
                "id": row.get::<i32, _>("id"),
                "full_name": row.get::<String, _>("full_name"),
                "short_name": row.get::<String, _>("short_name"),
                "url": row.get::<String, _>("url"),
            })
        },
    )
    .await
}

async fn rep_units(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    simple_list(
        &state,
        &uri,
        &headers,
        &method,
        "SELECT id, name, unit_type, multiplier FROM core_repetitionunit",
        "unit_type ASC, name ASC",
        &["id", "name", "unit_type", "multiplier"],
        |row| {
            json!({
                "id": row.get::<i32, _>("id"),
                "name": row.get::<String, _>("name"),
                "unit_type": row.get::<String, _>("unit_type"),
                "multiplier": row.get::<Option<i32>, _>("multiplier"),
            })
        },
    )
    .await
}

async fn weight_units(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    simple_list(
        &state,
        &uri,
        &headers,
        &method,
        "SELECT id, name FROM core_weightunit",
        "name ASC, id ASC",
        &["id", "name"],
        |row| {
            json!({
                "id": row.get::<i32, _>("id"),
                "name": row.get::<String, _>("name"),
            })
        },
    )
    .await
}

// The list helpers take the paging inputs one by one; bundling them would
// hide the SQL that each one builds.
#[allow(clippy::too_many_arguments)]
async fn simple_list<F>(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    select: &str,
    default_order: &str,
    allowed: &[&str],
    map: F,
) -> Response
where
    F: Fn(&sqlx::postgres::PgRow) -> Value,
{
    let pairs = query_pairs(uri);
    let mapq = qmap(&pairs);
    let page = page_from(&pairs);
    let order = httpx::ordering(
        mapq.get("ordering").map(|s| s.as_str()),
        allowed,
        default_order,
    );
    let count_sql = format!("SELECT COUNT(*) FROM ({select}) t");
    let count: i64 = match sqlx::query(sqlx::AssertSqlSafe(count_sql.clone()))
        .fetch_one(&state.pool)
        .await
    {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let data_sql = format!(
        "{select} ORDER BY {order} LIMIT {} OFFSET {}",
        page.limit, page.offset
    );
    let rows = match sqlx::query(sqlx::AssertSqlSafe(data_sql.clone()))
        .fetch_all(&state.pool)
        .await
    {
        Ok(rows) => rows,
        Err(err) => return httpx::db_error(err),
    };
    let results = rows.iter().map(map).collect();
    let body = paginated(uri, headers, state, count, &page, results);
    head_or_json(method, body)
}
