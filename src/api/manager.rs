//! Routines, sessions, logs, days, slots and set configs. Reads only.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::routing::MethodRouter;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{QueryBuilder, Row};
use uuid::Uuid;

use crate::engine::{date_sequence, structure_json, SequenceMode};
use crate::httpx::{self, head_or_json, page_from, paginated, qmap, query_pairs, CurrentUser};
use crate::jsonutil::{dec_str, opt_dec_str};
use crate::state::AppState;
use crate::timefmt::{fmt_date, fmt_dt};

use super::get_both;
use super::routine::load_routine;

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2/routine", routines));
    out.extend(get_both("/api/v2/routine/:id", routine_detail));
    out.extend(get_both("/api/v2/routine/:id/structure", routine_structure));
    out.extend(get_both(
        "/api/v2/routine/:id/date-sequence-display",
        routine_display,
    ));
    out.extend(get_both(
        "/api/v2/routine/:id/date-sequence-gym",
        routine_gym,
    ));
    out.extend(get_both("/api/v2/templates", templates));
    out.extend(get_both("/api/v2/templates/:id", template_detail));
    out.extend(get_both("/api/v2/public-templates", public_templates));
    out.extend(get_both(
        "/api/v2/public-templates/:id",
        public_template_detail,
    ));
    out.extend(get_both("/api/v2/workoutsession", sessions));
    out.extend(get_both("/api/v2/workoutsession/:id", session_detail));
    out.extend(get_both("/api/v2/workoutlog", logs));
    out.extend(get_both("/api/v2/workoutlog/:id", log_detail));
    out.extend(get_both("/api/v2/day", days));
    out.extend(get_both("/api/v2/day/:id", day_detail));
    out.extend(get_both("/api/v2/slot", slots));
    out.extend(get_both("/api/v2/slot/:id", slot_detail));
    out.extend(get_both("/api/v2/slot-entry", entries));
    out.extend(get_both("/api/v2/slot-entry/:id", entry_detail));
    out.extend(config_routes());
    out
}

#[derive(Clone, Copy)]
enum ConfigKind {
    Integer,
    Decimal(u32),
}

fn config_routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    out.extend(get_both("/api/v2/weight-config", weight_config));
    out.extend(get_both("/api/v2/weight-config/:id", weight_config_one));
    out.extend(get_both("/api/v2/max-weight-config", max_weight_config));
    out.extend(get_both(
        "/api/v2/max-weight-config/:id",
        max_weight_config_one,
    ));
    out.extend(get_both("/api/v2/repetitions-config", reps_config));
    out.extend(get_both("/api/v2/repetitions-config/:id", reps_config_one));
    out.extend(get_both("/api/v2/max-repetitions-config", max_reps_config));
    out.extend(get_both(
        "/api/v2/max-repetitions-config/:id",
        max_reps_config_one,
    ));
    out.extend(get_both("/api/v2/sets-config", sets_config));
    out.extend(get_both("/api/v2/sets-config/:id", sets_config_one));
    out.extend(get_both("/api/v2/max-sets-config", max_sets_config));
    out.extend(get_both("/api/v2/max-sets-config/:id", max_sets_config_one));
    out.extend(get_both("/api/v2/rest-config", rest_config));
    out.extend(get_both("/api/v2/rest-config/:id", rest_config_one));
    out.extend(get_both("/api/v2/max-rest-config", max_rest_config));
    out.extend(get_both("/api/v2/max-rest-config/:id", max_rest_config_one));
    out.extend(get_both("/api/v2/rir-config", rir_config));
    out.extend(get_both("/api/v2/rir-config/:id", rir_config_one));
    out.extend(get_both("/api/v2/max-rir-config", max_rir_config));
    out.extend(get_both("/api/v2/max-rir-config/:id", max_rir_config_one));
    out
}

macro_rules! config_handlers {
    ($list:ident, $one:ident, $table:literal, $kind:expr) => {
        async fn $list(
            State(state): State<AppState>,
            uri: Uri,
            headers: HeaderMap,
            method: Method,
        ) -> Response {
            config_list(&state, &uri, &headers, &method, $table, $kind).await
        }
        async fn $one(
            State(state): State<AppState>,
            headers: HeaderMap,
            Path(id): Path<i32>,
            method: Method,
        ) -> Response {
            config_one(&state, &headers, $table, id, $kind, &method).await
        }
    };
}

config_handlers!(
    weight_config,
    weight_config_one,
    "manager_weightconfig",
    ConfigKind::Decimal(2)
);
config_handlers!(
    max_weight_config,
    max_weight_config_one,
    "manager_maxweightconfig",
    ConfigKind::Decimal(2)
);
config_handlers!(
    reps_config,
    reps_config_one,
    "manager_repetitionsconfig",
    ConfigKind::Decimal(2)
);
config_handlers!(
    max_reps_config,
    max_reps_config_one,
    "manager_maxrepetitionsconfig",
    ConfigKind::Decimal(2)
);
config_handlers!(
    sets_config,
    sets_config_one,
    "manager_setsconfig",
    ConfigKind::Integer
);
config_handlers!(
    max_sets_config,
    max_sets_config_one,
    "manager_maxsetsconfig",
    ConfigKind::Integer
);
config_handlers!(
    rest_config,
    rest_config_one,
    "manager_restconfig",
    ConfigKind::Integer
);
config_handlers!(
    max_rest_config,
    max_rest_config_one,
    "manager_maxrestconfig",
    ConfigKind::Integer
);
config_handlers!(
    rir_config,
    rir_config_one,
    "manager_rirconfig",
    ConfigKind::Decimal(1)
);
config_handlers!(
    max_rir_config,
    max_rir_config_one,
    "manager_maxrirconfig",
    ConfigKind::Decimal(1)
);

async fn routines(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    routine_page(
        &state,
        &uri,
        &headers,
        &method,
        &user,
        RoutineScope::OwnOrPublic,
    )
    .await
}

async fn templates(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    routine_page(
        &state,
        &uri,
        &headers,
        &method,
        &user,
        RoutineScope::OwnTemplates,
    )
    .await
}

async fn public_templates(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let _ = user;
    routine_page(
        &state,
        &uri,
        &headers,
        &method,
        &user,
        RoutineScope::PublicTemplates,
    )
    .await
}

enum RoutineScope {
    OwnOrPublic,
    OwnTemplates,
    PublicTemplates,
}

async fn routine_page(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    user: &CurrentUser,
    scope: RoutineScope,
) -> Response {
    let pairs = query_pairs(uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let mut count_q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("SELECT COUNT(*) FROM manager_routine WHERE ");
    push_scope(&mut count_q, user, &scope);
    push_routine_filters(
        &mut count_q,
        &map,
        matches!(scope, RoutineScope::OwnOrPublic),
    );
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::ordering(
        map.get("ordering").map(|s| s.as_str()),
        &[
            "id",
            "name",
            "description",
            "created",
            "start",
            "end",
            "fit_in_week",
            "is_template",
            "is_public",
        ],
        "start DESC, created DESC, id DESC",
    );
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT id, name, description, created, start, \"end\", fit_in_week, is_template, is_public FROM manager_routine WHERE ",
    );
    push_scope(&mut data_q, user, &scope);
    push_routine_filters(
        &mut data_q,
        &map,
        matches!(scope, RoutineScope::OwnOrPublic),
    );
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
    let results = rows.iter().map(|row| routine_json(row, user.tz)).collect();
    head_or_json(
        method,
        paginated(uri, headers, state, count, &page, results),
    )
}

fn push_scope(q: &mut QueryBuilder<sqlx::Postgres>, user: &CurrentUser, scope: &RoutineScope) {
    match scope {
        RoutineScope::OwnOrPublic => {
            q.push("(user_id = ");
            q.push_bind(user.id);
            q.push(" OR is_public = true)");
        }
        RoutineScope::OwnTemplates => {
            q.push("is_template = true AND user_id = ");
            q.push_bind(user.id);
        }
        RoutineScope::PublicTemplates => {
            q.push("is_template = true AND is_public = true");
        }
    }
}

fn push_routine_filters(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    full: bool,
) {
    for (key, col) in [("name", "name"), ("description", "description")] {
        if let Some(value) = map.get(key) {
            q.push(format!(" AND {col} = "));
            q.push_bind(value.clone());
        }
    }
    if let Some(value) = map.get("created") {
        if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
            q.push(" AND created = ");
            q.push_bind(ts.with_timezone(&Utc));
        }
    }
    if full {
        if let Some(value) = map.get("start") {
            if let Ok(d) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
                q.push(" AND start = ");
                q.push_bind(d);
            }
        }
        if let Some(value) = map.get("end") {
            if let Ok(d) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
                q.push(" AND \"end\" = ");
                q.push_bind(d);
            }
        }
        if let Some(flag) = bool_q(map, "is_public") {
            q.push(" AND is_public = ");
            q.push_bind(flag);
        }
        if let Some(flag) = bool_q(map, "is_template") {
            q.push(" AND is_template = ");
            q.push_bind(flag);
        }
    }
}

fn bool_q(map: &std::collections::HashMap<String, String>, key: &str) -> Option<bool> {
    match map.get(key).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("true") | Some("1") => Some(true),
        Some("false") | Some("0") => Some(false),
        _ => None,
    }
}

fn routine_json(row: &PgRow, tz: chrono_tz::Tz) -> Value {
    let created: DateTime<Utc> = row.get("created");
    let start: NaiveDate = row.get("start");
    let end: NaiveDate = row.get("end");
    json!({
        "id": row.get::<i32, _>("id"),
        "name": row.get::<String, _>("name"),
        "description": row.get::<String, _>("description"),
        "created": fmt_dt(created, tz),
        "start": fmt_date(start),
        "end": fmt_date(end),
        "fit_in_week": row.get::<bool, _>("fit_in_week"),
        "is_template": row.get::<bool, _>("is_template"),
        "is_public": row.get::<bool, _>("is_public"),
    })
}

async fn routine_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    routine_one(&state, &headers, id, &method, false).await
}

async fn template_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    routine_one(&state, &headers, id, &method, true).await
}

async fn public_template_detail(
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
        "SELECT id, name, description, created, start, \"end\", fit_in_week, is_template, is_public, user_id FROM manager_routine WHERE id = $1 AND is_template AND is_public",
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
    let _ = user;
    head_or_json(&method, routine_json(&row, user.tz))
}

async fn routine_one(
    state: &AppState,
    headers: &HeaderMap,
    id: i32,
    method: &Method,
    template: bool,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let row = match sqlx::query(
        "SELECT id, name, description, created, start, \"end\", fit_in_week, is_template, is_public, user_id FROM manager_routine WHERE id = $1",
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
    let owner: i32 = row.get("user_id");
    let is_template: bool = row.get("is_template");
    let is_public: bool = row.get("is_public");
    if template && (!is_template || owner != user.id) {
        return httpx::not_found();
    }
    if owner != user.id {
        if is_public && is_template {
            // readable
        } else if is_public {
            return httpx::permission_denied();
        } else {
            return httpx::not_found();
        }
    }
    head_or_json(method, routine_json(&row, user.tz))
}

async fn routine_structure(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    sequence_response(&state, &headers, id, &method, Seq::Structure).await
}

async fn routine_display(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    sequence_response(&state, &headers, id, &method, Seq::Display).await
}

async fn routine_gym(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i32>,
    method: Method,
) -> Response {
    sequence_response(&state, &headers, id, &method, Seq::Gym).await
}

enum Seq {
    Structure,
    Display,
    Gym,
}

async fn sequence_response(
    state: &AppState,
    headers: &HeaderMap,
    id: i32,
    method: &Method,
    kind: Seq,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let access = match sqlx::query(
        "SELECT user_id, is_template, is_public FROM manager_routine WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(access) = access else {
        return httpx::not_found();
    };
    let owner: i32 = access.get("user_id");
    let is_template: bool = access.get("is_template");
    let is_public: bool = access.get("is_public");
    if owner != user.id {
        if !(is_public && is_template) {
            if is_public {
                return httpx::permission_denied();
            }
            return httpx::not_found();
        }
    }
    let routine = match load_routine(&state.pool, id, user.tz).await {
        Ok(Some(routine)) => routine,
        Ok(None) => return httpx::not_found(),
        Err(err) => return httpx::db_error(err),
    };
    let today = Utc::now().with_timezone(&user.tz).date_naive();
    let body = match kind {
        Seq::Structure => structure_json(&routine),
        Seq::Display => Value::Array(date_sequence(&routine, today, SequenceMode::Display)),
        Seq::Gym => Value::Array(date_sequence(&routine, today, SequenceMode::Gym)),
    };
    head_or_json(method, body)
}

async fn sessions(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    owned_list(&state, &uri, &headers, &method, OwnedKind::Session).await
}
async fn session_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    owned_one(&state, &headers, OwnedKind::Session, &id, &method).await
}
async fn logs(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    owned_list(&state, &uri, &headers, &method, OwnedKind::Log).await
}
async fn log_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    owned_one(&state, &headers, OwnedKind::Log, &id, &method).await
}
async fn days(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    owned_list(&state, &uri, &headers, &method, OwnedKind::Day).await
}
async fn day_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    owned_one(&state, &headers, OwnedKind::Day, &id, &method).await
}
async fn slots(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    owned_list(&state, &uri, &headers, &method, OwnedKind::Slot).await
}
async fn slot_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    owned_one(&state, &headers, OwnedKind::Slot, &id, &method).await
}
async fn entries(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
) -> Response {
    owned_list(&state, &uri, &headers, &method, OwnedKind::Entry).await
}
async fn entry_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    owned_one(&state, &headers, OwnedKind::Entry, &id, &method).await
}

#[derive(Clone, Copy)]
enum OwnedKind {
    Session,
    Log,
    Day,
    Slot,
    Entry,
}

fn owned_sql(kind: OwnedKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        OwnedKind::Session => (
            "manager_workoutsession s",
            "s.user_id = ",
            "s.datetime_start ASC, s.id ASC",
        ),
        OwnedKind::Log => (
            "manager_workoutlog s",
            "s.user_id = ",
            "s.date ASC, s.repetitions ASC NULLS FIRST, s.weight ASC NULLS FIRST, s.id ASC",
        ),
        OwnedKind::Day => (
            "manager_day s JOIN manager_routine r ON r.id = s.routine_id",
            "r.user_id = ",
            "s.\"order\" ASC, s.id ASC",
        ),
        OwnedKind::Slot => (
            "manager_slot s JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id",
            "r.user_id = ",
            "s.\"order\" ASC, s.id ASC",
        ),
        OwnedKind::Entry => (
            "manager_slotentry s JOIN manager_slot sl ON sl.id = s.slot_id JOIN manager_day d ON d.id = sl.day_id JOIN manager_routine r ON r.id = d.routine_id",
            "r.user_id = ",
            "s.\"order\" ASC, s.id ASC",
        ),
    }
}

async fn owned_list(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    kind: OwnedKind,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let (from, owner_sql, default_order) = owned_sql(kind);
    let mut count_q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new(format!("SELECT COUNT(*) FROM {from} WHERE {owner_sql}"));
    count_q.push_bind(user.id);
    if let Err(resp) = push_owned_filters(&mut count_q, kind, &map, user.tz) {
        return resp;
    }
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let allowed: &[&str] = match kind {
        OwnedKind::Session => &[
            "id",
            "routine",
            "day",
            "notes",
            "impression",
            "datetime_start",
            "datetime_end",
        ],
        OwnedKind::Log => &[
            "id",
            "date",
            "session",
            "routine",
            "iteration",
            "exercise",
            "repetitions",
            "weight",
            "rir",
        ],
        OwnedKind::Day => &[
            "id",
            "routine",
            "order",
            "name",
            "description",
            "is_rest",
            "need_logs_to_advance",
        ],
        OwnedKind::Slot => &["id", "day", "order", "comment"],
        OwnedKind::Entry => &["id", "slot", "exercise", "order", "comment", "type"],
    };
    let order = httpx::ordering(
        map.get("ordering").map(|s| s.as_str()),
        allowed,
        default_order,
    );
    let order = qualify_order(&order);
    let select = owned_select(kind);
    let mut data_q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new(format!("SELECT {select} FROM {from} WHERE {owner_sql}"));
    data_q.push_bind(user.id);
    if let Err(resp) = push_owned_filters(&mut data_q, kind, &map, user.tz) {
        return resp;
    }
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
        .map(|row| owned_json(kind, row, user.tz))
        .collect();
    head_or_json(
        method,
        paginated(uri, headers, state, count, &page, results),
    )
}

fn qualify_order(order: &str) -> String {
    // Default orders are already qualified or quoted. Client orders are bare
    // column names from the whitelist; prefix them so joins stay unambiguous.
    if order.contains('.') || order.contains('"') {
        return order.to_string();
    }
    order
        .split(", ")
        .map(|part| {
            let (name, dir) = part.split_once(' ').unwrap_or((part, "ASC"));
            let col = match name {
                "order" => "s.\"order\"".to_string(),
                "end" => "s.\"end\"".to_string(),
                "routine" => "s.routine_id".to_string(),
                "day" => "s.day_id".to_string(),
                "session" => "s.session_id".to_string(),
                "slot" => "s.slot_id".to_string(),
                "slot_entry" => "s.slot_entry_id".to_string(),
                "exercise" => "s.exercise_id".to_string(),
                other => format!("s.{other}"),
            };
            format!("{col} {dir}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn owned_select(kind: OwnedKind) -> &'static str {
    match kind {
        OwnedKind::Session => "s.id, s.routine_id, s.day_id, s.notes, s.impression, s.datetime_start, s.datetime_end",
        OwnedKind::Log => "s.id, s.date, s.session_id, s.routine_id, s.iteration, s.slot_entry_id, s.next_log_id, s.exercise_id, s.repetitions_unit_id, s.repetitions, s.repetitions_target, s.weight_unit_id, s.weight, s.weight_target, s.rir, s.rir_target, s.rest, s.rest_target",
        OwnedKind::Day => "s.id, s.routine_id, s.\"order\", s.name, s.description, s.is_rest, s.need_logs_to_advance, s.type, s.config",
        OwnedKind::Slot => "s.id, s.day_id, s.\"order\", s.comment, s.config",
        OwnedKind::Entry => "s.id, s.slot_id, s.exercise_id, s.type, s.repetition_unit_id, s.repetition_rounding, s.weight_unit_id, s.weight_rounding, s.\"order\", s.comment, s.config",
    }
}

fn push_owned_filters(
    q: &mut QueryBuilder<sqlx::Postgres>,
    kind: OwnedKind,
    map: &std::collections::HashMap<String, String>,
    tz: chrono_tz::Tz,
) -> Result<(), Response> {
    let _ = tz;
    match kind {
        OwnedKind::Session => {
            eq_i32(q, map, "routine", "s.routine_id");
            eq_i32(q, map, "day", "s.day_id");
            eq_str(q, map, "notes", "s.notes");
            eq_str(q, map, "impression", "s.impression");
            dt_filters(q, map, "datetime_start", "s.datetime_start");
            dt_filters(q, map, "datetime_end", "s.datetime_end");
        }
        OwnedKind::Log => {
            eq_i32(q, map, "routine", "s.routine_id");
            eq_uuid(q, map, "session", "s.session_id");
            eq_i32(q, map, "exercise", "s.exercise_id");
            in_i32(q, map, "exercise__in", "s.exercise_id");
            eq_i32(q, map, "iteration", "s.iteration");
            dt_filters(q, map, "date", "s.date");
        }
        OwnedKind::Day => {
            eq_i32(q, map, "routine", "s.routine_id");
            eq_i32(q, map, "id", "s.id");
            eq_i32(q, map, "order", "s.\"order\"");
            eq_str(q, map, "name", "s.name");
            eq_bool(q, map, "is_rest", "s.is_rest");
            eq_bool(q, map, "need_logs_to_advance", "s.need_logs_to_advance");
        }
        OwnedKind::Slot => {
            eq_i32(q, map, "day", "s.day_id");
            eq_i32(q, map, "order", "s.\"order\"");
            eq_str(q, map, "comment", "s.comment");
        }
        OwnedKind::Entry => {
            eq_i32(q, map, "slot", "s.slot_id");
            eq_i32(q, map, "exercise", "s.exercise_id");
        }
    }
    Ok(())
}

fn eq_i32(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    if let Some(value) = map.get(key).and_then(|v| v.parse::<i32>().ok()) {
        q.push(format!(" AND {col} = "));
        q.push_bind(value);
    }
}

fn in_i32(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    let values = map.get(key).map(|v| {
        v.split(',')
            .filter_map(|p| p.trim().parse::<i32>().ok())
            .collect::<Vec<_>>()
    });
    if let Some(values) = values {
        if values.is_empty() {
            q.push(" AND false");
        } else {
            q.push(format!(" AND {col} = ANY("));
            q.push_bind(values);
            q.push(")");
        }
    }
}

fn eq_uuid(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    if let Some(value) = map.get(key).and_then(|v| Uuid::parse_str(v).ok()) {
        q.push(format!(" AND {col} = "));
        q.push_bind(value);
    }
}

fn eq_str(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    if let Some(value) = map.get(key) {
        q.push(format!(" AND {col} = "));
        q.push_bind(value.clone());
    }
}

fn eq_bool(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    if let Some(flag) = bool_q(map, key) {
        q.push(format!(" AND {col} = "));
        q.push_bind(flag);
    }
}

fn dt_filters(
    q: &mut QueryBuilder<sqlx::Postgres>,
    map: &std::collections::HashMap<String, String>,
    key: &str,
    col: &str,
) {
    if let Some(value) = map
        .get(&format!("{key}__date"))
        .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
    {
        q.push(format!(" AND ({col} AT TIME ZONE 'UTC')::date = "));
        q.push_bind(value);
    }
    for (suffix, op) in [
        ("__gt", ">"),
        ("__gte", ">="),
        ("__lt", "<"),
        ("__lte", "<="),
    ] {
        let param = format!("{key}{suffix}");
        if let Some(value) = map.get(&param) {
            if let Ok(ts) = parse_dt(value) {
                q.push(format!(" AND {col} {op} "));
                q.push_bind(ts);
            }
        }
    }
    if let Some(value) = map.get(key) {
        if let Ok(ts) = parse_dt(value) {
            q.push(format!(" AND {col} = "));
            q.push_bind(ts);
        }
    }
}

fn parse_dt(value: &str) -> Result<DateTime<Utc>, ()> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Ok(ts.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc());
    }
    Err(())
}

fn owned_json(kind: OwnedKind, row: &PgRow, tz: chrono_tz::Tz) -> Value {
    match kind {
        OwnedKind::Session => {
            let start: DateTime<Utc> = row.get("datetime_start");
            let end: Option<DateTime<Utc>> = row.get("datetime_end");
            json!({
                "id": row.get::<Uuid, _>("id"),
                "routine": row.get::<Option<i32>, _>("routine_id"),
                "day": row.get::<Option<i32>, _>("day_id"),
                "notes": row.get::<String, _>("notes"),
                "impression": row.get::<String, _>("impression"),
                "datetime_start": fmt_dt(start, tz),
                "datetime_end": end.map(|t| fmt_dt(t, tz)),
            })
        }
        OwnedKind::Log => json!({
            "id": row.get::<Uuid, _>("id"),
            "date": fmt_dt(row.get::<DateTime<Utc>, _>("date"), tz),
            "session": row.get::<Option<Uuid>, _>("session_id"),
            "routine": row.get::<Option<i32>, _>("routine_id"),
            "iteration": row.get::<Option<i32>, _>("iteration"),
            "slot_entry": row.get::<Option<i32>, _>("slot_entry_id"),
            "next_log": row.get::<Option<Uuid>, _>("next_log_id"),
            "exercise": row.get::<Option<i32>, _>("exercise_id"),
            "repetitions_unit": row.get::<Option<i32>, _>("repetitions_unit_id"),
            "repetitions": opt_dec_str(row.get("repetitions"), 2),
            "repetitions_target": opt_dec_str(row.get("repetitions_target"), 2),
            "weight_unit": row.get::<Option<i32>, _>("weight_unit_id"),
            "weight": opt_dec_str(row.get("weight"), 2),
            "weight_target": opt_dec_str(row.get("weight_target"), 2),
            "rir": opt_dec_str(row.get("rir"), 1),
            "rir_target": opt_dec_str(row.get("rir_target"), 1),
            "rest": row.get::<Option<i32>, _>("rest"),
            "rest_target": row.get::<Option<i32>, _>("rest_target"),
        }),
        OwnedKind::Day => json!({
            "id": row.get::<i32, _>("id"),
            "routine": row.get::<i32, _>("routine_id"),
            "order": row.get::<i32, _>("order"),
            "name": row.get::<String, _>("name"),
            "description": row.get::<String, _>("description"),
            "is_rest": row.get::<bool, _>("is_rest"),
            "need_logs_to_advance": row.get::<bool, _>("need_logs_to_advance"),
            "type": row.get::<String, _>("type"),
            "config": json_value(row),
        }),
        OwnedKind::Slot => json!({
            "id": row.get::<i32, _>("id"),
            "day": row.get::<i32, _>("day_id"),
            "order": row.get::<i32, _>("order"),
            "comment": row.get::<String, _>("comment"),
            "config": json_value(row),
        }),
        OwnedKind::Entry => json!({
            "id": row.get::<i32, _>("id"),
            "slot": row.get::<i32, _>("slot_id"),
            "exercise": row.get::<i32, _>("exercise_id"),
            "type": row.get::<String, _>("type"),
            "repetition_unit": row.get::<Option<i32>, _>("repetition_unit_id"),
            "repetition_rounding": opt_dec_str(row.get("repetition_rounding"), 2),
            "weight_unit": row.get::<Option<i32>, _>("weight_unit_id"),
            "weight_rounding": opt_dec_str(row.get("weight_rounding"), 2),
            "order": row.get::<i32, _>("order"),
            "comment": row.get::<String, _>("comment"),
            "config": json_value(row),
        }),
    }
}

fn json_value(row: &PgRow) -> Value {
    let value: Option<sqlx::types::Json<Value>> = row.try_get("config").unwrap_or(None);
    value.map(|j| j.0).unwrap_or(Value::Null)
}

async fn owned_one(
    state: &AppState,
    headers: &HeaderMap,
    kind: OwnedKind,
    id: &str,
    method: &Method,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let (from, owner_sql, _) = owned_sql(kind);
    let select = owned_select(kind);
    let mut q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new(format!("SELECT {select} FROM {from} WHERE {owner_sql}"));
    q.push_bind(user.id);
    q.push(" AND s.id = ");
    match kind {
        OwnedKind::Session | OwnedKind::Log => match Uuid::parse_str(id) {
            Ok(id) => q.push_bind(id),
            Err(_) => return httpx::not_found(),
        },
        _ => match id.parse::<i32>() {
            Ok(id) => q.push_bind(id),
            Err(_) => return httpx::not_found(),
        },
    };
    let row = match q.build().fetch_optional(&state.pool).await {
        Ok(row) => row,
        Err(err) => return httpx::db_error(err),
    };
    let Some(row) = row else {
        return httpx::not_found();
    };
    head_or_json(method, owned_json(kind, &row, user.tz))
}

async fn config_list(
    state: &AppState,
    uri: &Uri,
    headers: &HeaderMap,
    method: &Method,
    table: &str,
    kind: ConfigKind,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let pairs = query_pairs(uri);
    let map = qmap(&pairs);
    let page = page_from(&pairs);
    let from = format!(
        "{table} s JOIN manager_slotentry e ON e.id = s.slot_entry_id JOIN manager_slot sl ON sl.id = e.slot_id JOIN manager_day d ON d.id = sl.day_id JOIN manager_routine r ON r.id = d.routine_id"
    );
    let mut count_q: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new(format!("SELECT COUNT(*) FROM {from} WHERE r.user_id = "));
    count_q.push_bind(user.id);
    eq_i32(&mut count_q, &map, "slot_entry", "s.slot_entry_id");
    eq_i32(&mut count_q, &map, "iteration", "s.iteration");
    eq_str(&mut count_q, &map, "operation", "s.operation");
    eq_str(&mut count_q, &map, "step", "s.step");
    let count: i64 = match count_q.build().fetch_one(&state.pool).await {
        Ok(row) => row.get(0),
        Err(err) => return httpx::db_error(err),
    };
    let order = httpx::ordering(
        map.get("ordering").map(|s| s.as_str()),
        &[
            "id",
            "slot_entry",
            "iteration",
            "value",
            "operation",
            "step",
            "repeat",
        ],
        "s.slot_entry_id ASC, s.iteration ASC, s.id ASC",
    );
    let order = if order.contains('.') {
        order
    } else {
        qualify_order(&order)
    };
    let mut data_q: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(format!(
        "SELECT s.id, s.slot_entry_id, s.iteration, s.value, s.operation, s.step, s.repeat, s.requirements FROM {from} WHERE r.user_id = "
    ));
    data_q.push_bind(user.id);
    eq_i32(&mut data_q, &map, "slot_entry", "s.slot_entry_id");
    eq_i32(&mut data_q, &map, "iteration", "s.iteration");
    eq_str(&mut data_q, &map, "operation", "s.operation");
    eq_str(&mut data_q, &map, "step", "s.step");
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
    let results = rows.iter().map(|row| config_json(row, kind)).collect();
    head_or_json(
        method,
        paginated(uri, headers, state, count, &page, results),
    )
}

async fn config_one(
    state: &AppState,
    headers: &HeaderMap,
    table: &str,
    id: i32,
    kind: ConfigKind,
    method: &Method,
) -> Response {
    let user = match httpx::require_user(state, headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let sql = format!(
        r#"SELECT s.id, s.slot_entry_id, s.iteration, s.value, s.operation, s.step, s.repeat, s.requirements
           FROM {table} s
           JOIN manager_slotentry e ON e.id = s.slot_entry_id
           JOIN manager_slot sl ON sl.id = e.slot_id
           JOIN manager_day d ON d.id = sl.day_id
           JOIN manager_routine r ON r.id = d.routine_id
           WHERE r.user_id = $1 AND s.id = $2"#
    );
    let row = match sqlx::query(&sql)
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
    head_or_json(method, config_json(&row, kind))
}

fn config_json(row: &PgRow, kind: ConfigKind) -> Value {
    let value = match kind {
        ConfigKind::Integer => json!(row.get::<i32, _>("value")),
        ConfigKind::Decimal(scale) => Value::String(dec_str(row.get::<Decimal, _>("value"), scale)),
    };
    let requirements: Option<sqlx::types::Json<Value>> =
        row.try_get("requirements").unwrap_or(None);
    json!({
        "id": row.get::<i32, _>("id"),
        "slot_entry": row.get::<i32, _>("slot_entry_id"),
        "iteration": row.get::<i32, _>("iteration"),
        "value": value,
        "operation": row.get::<String, _>("operation"),
        "step": row.get::<String, _>("step"),
        "repeat": row.get::<bool, _>("repeat"),
        "requirements": requirements.map(|j| j.0).unwrap_or(Value::Null),
    })
}

// ---- render hooks for REST writes (responses identical to detail GETs) ----
pub(super) async fn render_routine(state: &AppState, headers: &HeaderMap, id: String) -> Response {
    match id.parse::<i32>() {
        Ok(id) => routine_one(state, headers, id, &Method::GET, false).await,
        Err(_) => httpx::not_found(),
    }
}

pub(super) async fn render_owned(
    state: &AppState,
    headers: &HeaderMap,
    kind: &str,
    id: String,
) -> Response {
    let kind = match kind {
        "day" => OwnedKind::Day,
        "slot" => OwnedKind::Slot,
        "entry" => OwnedKind::Entry,
        "session" => OwnedKind::Session,
        _ => OwnedKind::Log,
    };
    owned_one(state, headers, kind, &id, &Method::GET).await
}

pub(super) async fn render_config(
    state: &AppState,
    headers: &HeaderMap,
    table: &'static str,
    id: String,
) -> Response {
    let kind = match table {
        "manager_setsconfig"
        | "manager_maxsetsconfig"
        | "manager_restconfig"
        | "manager_maxrestconfig" => ConfigKind::Integer,
        "manager_rirconfig" | "manager_maxrirconfig" => ConfigKind::Decimal(1),
        _ => ConfigKind::Decimal(2),
    };
    match id.parse::<i32>() {
        Ok(id) => config_one(state, headers, table, id, kind, &Method::GET).await,
        Err(_) => httpx::not_found(),
    }
}
