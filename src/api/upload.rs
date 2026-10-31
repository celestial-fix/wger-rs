//! PowerSync push endpoint: PUT/PATCH/DELETE /api/v2/upload-powersync-data.
//!
//! Mirrors wger 2.7's `upload_powersync_data` view and `PowerSyncHandler`
//! dispatch: permanent refusals are 200 `{error, details?}` because PowerSync
//! treats non-2xx as retry; only transient infrastructure errors are 503 and
//! unexpected ones 500. Unauthenticated is 403.

use axum::extract::{Json, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{put, MethodRouter};
use chrono::{NaiveDate, NaiveTime};
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use sqlx::{PgPool, QueryBuilder, Row};
use uuid::Uuid;

use crate::httpx::{self, ok_json};
use crate::state::AppState;

const CATEGORY_NAMESPACE: Uuid = Uuid::from_u128(0x4c5e_f6dd_97c9_5b18_9f8b_2a5c_1ed7_0a2f);

const IMPRESSIONS: [&str; 3] = ["1", "2", "3"];

#[derive(Clone, Copy)]
enum Pk {
    Uuid,
    I32,
}

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Uuid,
    I32,
    Dec,
    Bool,
    Str,
    Date,
    Ts,
    Time,
    Json,
}

/// How a row's ownership is enforced for UPDATE/DELETE.
enum Owned {
    /// `id = <pk> AND user_id = <user>`
    Direct,
    /// `<fk> IN (<subquery>; a `user_id` bind is appended to the subquery)`
    Via(&'static str, &'static str),
}

struct Col {
    field: &'static str,
    column: &'static str,
    ty: Ty,
    on_create: bool,
    on_update: bool,
    nullable: bool,
    /// SQL literal used when the field is absent (or read-only) on create.
    default: Option<&'static str>,
    /// Never taken from the payload; create always uses the default.
    readonly: bool,
}

struct Fk {
    field: &'static str,
    ty: Ty,
    /// `SELECT user_id FROM ... WHERE id = $1` — missing or foreign → Forbidden.
    owner: Option<&'static str>,
    /// `SELECT 1 FROM ... WHERE id = $1` — missing → field validation error.
    exists: Option<&'static str>,
}

struct TableSpec {
    table: &'static str,
    label: &'static str,
    pk: Pk,
    create: bool,
    delete: bool,
    /// Column forced to the caller's id on INSERT (models with a direct user FK).
    force_user: Option<&'static str>,
    cols: &'static [Col],
    fks: &'static [Fk],
    /// `SELECT [extra cols] FROM ... WHERE id = $1 AND user_id = $2`.
    owned_select: &'static str,
    owned: Owned,
    /// Payload keys dropped before validation (read-only or binary fields).
    drop: &'static [&'static str],
}

macro_rules! col {
    ($field:literal, $column:literal, $ty:expr, $on_create:expr, $on_update:expr, $nullable:expr) => {
        Col {
            field: $field,
            column: $column,
            ty: $ty,
            on_create: $on_create,
            on_update: $on_update,
            nullable: $nullable,
            default: None,
            readonly: false,
        }
    };
    ($field:literal, $column:literal, $ty:expr, $on_create:expr, $on_update:expr, $nullable:expr, $default:literal) => {
        Col {
            field: $field,
            column: $column,
            ty: $ty,
            on_create: $on_create,
            on_update: $on_update,
            nullable: $nullable,
            default: Some($default),
            readonly: false,
        }
    };
    ($field:literal, $column:literal, $ty:expr, $on_create:expr, $on_update:expr, $nullable:expr, $default:literal, readonly) => {
        Col {
            field: $field,
            column: $column,
            ty: $ty,
            on_create: $on_create,
            on_update: $on_update,
            nullable: $nullable,
            default: Some($default),
            readonly: true,
        }
    };
}

const CATEGORY_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("name", "name", Ty::Str, true, true, false),
    col!("unit", "unit", Ty::Str, true, true, false, "''"),
    col!(
        "metric_type",
        "metric_type",
        Ty::Str,
        true,
        true,
        false,
        "'custom'"
    ),
    col!("chart_type", "chart_type", Ty::Str, true, true, true),
    col!(
        "chart_config",
        "chart_config",
        Ty::Json,
        true,
        true,
        false,
        "'{}'"
    ),
    col!("parent", "parent_id", Ty::Uuid, true, true, true),
    col!("order", "order", Ty::I32, true, true, false, "0"),
    col!(
        "is_official",
        "is_official",
        Ty::Bool,
        true,
        false,
        false,
        "false",
        readonly
    ),
    col!(
        "dynamic_type",
        "dynamic_type",
        Ty::Str,
        true,
        true,
        false,
        "'none'"
    ),
    col!(
        "dynamic_params",
        "dynamic_params",
        Ty::Json,
        true,
        true,
        false,
        "'{}'"
    ),
];

const MEASUREMENT_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("category", "category_id", Ty::Uuid, true, true, false),
    col!("date", "date", Ty::Ts, true, true, false, "now()"),
    col!("value", "value", Ty::Dec, true, true, false),
    col!("notes", "notes", Ty::Str, true, true, false, "''"),
    col!("source", "source", Ty::Str, true, true, false, "'user'"),
    col!("external_id", "external_id", Ty::Uuid, true, true, true),
    col!(
        "extra_data",
        "extra_data",
        Ty::Json,
        true,
        true,
        false,
        "'{}'"
    ),
];

const PROFILE_COLS: &[Col] = &[
    col!(
        "weight_rounding",
        "weight_rounding",
        Ty::Dec,
        false,
        true,
        true
    ),
    col!(
        "repetitions_rounding",
        "repetitions_rounding",
        Ty::Dec,
        false,
        true,
        true
    ),
    col!(
        "workout_reminder_active",
        "workout_reminder_active",
        Ty::Bool,
        false,
        true,
        false
    ),
    col!(
        "workout_reminder",
        "workout_reminder",
        Ty::I32,
        false,
        true,
        false
    ),
    col!(
        "workout_duration",
        "workout_duration",
        Ty::I32,
        false,
        true,
        false
    ),
    col!(
        "last_workout_notification",
        "last_workout_notification",
        Ty::Date,
        false,
        true,
        true
    ),
    col!(
        "notification_language",
        "notification_language_id",
        Ty::I32,
        false,
        true,
        false
    ),
    col!("age", "age", Ty::I32, false, true, true),
    col!("birthdate", "birthdate", Ty::Date, false, true, true),
    col!("height", "height", Ty::I32, false, true, true),
    col!("gender", "gender", Ty::Str, false, true, true),
    col!("sleep_hours", "sleep_hours", Ty::I32, false, true, true),
    col!("work_hours", "work_hours", Ty::I32, false, true, true),
    col!(
        "work_intensity",
        "work_intensity",
        Ty::Str,
        false,
        true,
        true
    ),
    col!("sport_hours", "sport_hours", Ty::I32, false, true, true),
    col!(
        "sport_intensity",
        "sport_intensity",
        Ty::Str,
        false,
        true,
        true
    ),
    col!(
        "freetime_hours",
        "freetime_hours",
        Ty::I32,
        false,
        true,
        true
    ),
    col!(
        "freetime_intensity",
        "freetime_intensity",
        Ty::Str,
        false,
        true,
        true
    ),
    col!("calories", "calories", Ty::I32, false, true, true),
    col!("weight_unit", "weight_unit", Ty::Str, false, true, false),
    col!(
        "num_days_weight_reminder",
        "num_days_weight_reminder",
        Ty::I32,
        false,
        true,
        false
    ),
    col!("time_zone", "time_zone", Ty::Str, false, true, false),
];

const WORKOUTLOG_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("date", "date", Ty::Ts, true, true, false, "now()"),
    col!("session", "session_id", Ty::Uuid, true, true, true),
    col!("routine", "routine_id", Ty::I32, true, true, true),
    col!("iteration", "iteration", Ty::I32, true, true, true),
    col!("slot_entry", "slot_entry_id", Ty::I32, true, true, true),
    col!("next_log", "next_log_id", Ty::Uuid, true, true, true),
    col!("exercise", "exercise_id", Ty::I32, true, true, false),
    col!(
        "repetitions_unit",
        "repetitions_unit_id",
        Ty::I32,
        true,
        true,
        true,
        "1"
    ),
    col!("repetitions", "repetitions", Ty::Dec, true, true, true),
    col!(
        "repetitions_target",
        "repetitions_target",
        Ty::Dec,
        true,
        true,
        true
    ),
    col!(
        "weight_unit",
        "weight_unit_id",
        Ty::I32,
        true,
        true,
        true,
        "1"
    ),
    col!("weight", "weight", Ty::Dec, true, true, true),
    col!("weight_target", "weight_target", Ty::Dec, true, true, true),
    col!("rir", "rir", Ty::Dec, true, true, true),
    col!("rir_target", "rir_target", Ty::Dec, true, true, true),
    col!("rest", "rest", Ty::I32, true, true, true),
    col!("rest_target", "rest_target", Ty::I32, true, true, true),
];

const WORKOUTSESSION_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("routine", "routine_id", Ty::I32, true, true, true),
    col!("day", "day_id", Ty::I32, true, true, true),
    col!("notes", "notes", Ty::Str, true, true, true),
    col!(
        "impression",
        "impression",
        Ty::Str,
        true,
        true,
        false,
        "'2'"
    ),
    col!(
        "datetime_start",
        "datetime_start",
        Ty::Ts,
        true,
        true,
        false
    ),
    col!("datetime_end", "datetime_end", Ty::Ts, true, true, true),
];

const ROUTINE_COLS: &[Col] = &[
    col!("id", "id", Ty::I32, false, false, false),
    col!("name", "name", Ty::Str, false, true, false),
    col!("description", "description", Ty::Str, false, true, true),
    col!("created", "created", Ty::Ts, false, true, false),
    col!("start", "start", Ty::Date, false, true, true),
    col!("end", "end", Ty::Date, false, true, true),
    col!("fit_in_week", "fit_in_week", Ty::Bool, false, true, false),
    col!("is_template", "is_template", Ty::Bool, false, true, false),
    col!("is_public", "is_public", Ty::Bool, false, true, false),
];

const GALLERY_COLS: &[Col] = &[
    col!("id", "id", Ty::I32, false, false, false),
    col!("date", "date", Ty::Date, false, true, false),
    col!("description", "description", Ty::Str, false, true, true),
    col!("height", "height", Ty::I32, false, true, true),
    col!("width", "width", Ty::I32, false, true, true),
];

const PLAN_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!(
        "creation_date",
        "creation_date",
        Ty::Date,
        true,
        false,
        false,
        "CURRENT_DATE",
        readonly
    ),
    col!("start", "start", Ty::Date, true, true, true, "CURRENT_DATE"),
    col!("end", "end", Ty::Date, true, true, true),
    col!(
        "description",
        "description",
        Ty::Str,
        true,
        true,
        false,
        "''"
    ),
    col!(
        "only_logging",
        "only_logging",
        Ty::Bool,
        true,
        true,
        false,
        "false"
    ),
    col!("goal_energy", "goal_energy", Ty::I32, true, true, true),
    col!("goal_protein", "goal_protein", Ty::I32, true, true, true),
    col!(
        "goal_carbohydrates",
        "goal_carbohydrates",
        Ty::I32,
        true,
        true,
        true
    ),
    col!("goal_fat", "goal_fat", Ty::I32, true, true, true),
    col!("goal_fiber", "goal_fiber", Ty::I32, true, true, true),
    col!(
        "has_goal_calories",
        "has_goal_calories",
        Ty::Bool,
        true,
        false,
        false,
        "false",
        readonly
    ),
];

const MEAL_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("plan", "plan_id", Ty::Uuid, true, true, false),
    col!("order", "order", Ty::I32, true, false, false, "1"),
    col!("time", "time", Ty::Time, true, true, true),
    col!("name", "name", Ty::Str, true, true, false, "''"),
];

const MEALITEM_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("meal", "meal_id", Ty::Uuid, true, true, false),
    col!("ingredient", "ingredient_id", Ty::I32, true, true, false),
    col!("weight_unit", "weight_unit_id", Ty::I32, true, true, true),
    col!("order", "order", Ty::I32, true, false, false, "1"),
    col!("amount", "amount", Ty::Dec, true, true, false),
];

const LOGITEM_COLS: &[Col] = &[
    col!("id", "id", Ty::Uuid, true, false, false),
    col!("plan", "plan_id", Ty::Uuid, true, true, false),
    col!("meal", "meal_id", Ty::Uuid, true, true, true),
    col!("ingredient", "ingredient_id", Ty::I32, true, true, false),
    col!("weight_unit", "weight_unit_id", Ty::I32, true, true, true),
    col!("datetime", "datetime", Ty::Ts, true, true, false, "now()"),
    col!("amount", "amount", Ty::Dec, true, true, false),
];

const CATEGORY_FKS: &[Fk] = &[Fk {
    field: "parent",
    ty: Ty::Uuid,
    owner: Some("SELECT user_id FROM measurements_category WHERE id = $1"),
    exists: None,
}];

const MEASUREMENT_FKS: &[Fk] = &[Fk {
    field: "category",
    ty: Ty::Uuid,
    owner: Some("SELECT user_id FROM measurements_category WHERE id = $1"),
    exists: None,
}];

const PROFILE_FKS: &[Fk] = &[Fk {
    field: "notification_language",
    ty: Ty::I32,
    owner: None,
    exists: Some("SELECT 1 FROM core_language WHERE id = $1"),
}];

const WORKOUTLOG_FKS: &[Fk] = &[
    Fk {
        field: "routine",
        ty: Ty::I32,
        owner: Some("SELECT user_id FROM manager_routine WHERE id = $1"),
        exists: None,
    },
    Fk {
        field: "session",
        ty: Ty::Uuid,
        owner: Some("SELECT user_id FROM manager_workoutsession WHERE id = $1"),
        exists: None,
    },
    Fk {
        field: "slot_entry",
        ty: Ty::I32,
        owner: Some(
            "SELECT r.user_id FROM manager_slotentry se JOIN manager_slot s ON s.id = se.slot_id JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id WHERE se.id = $1",
        ),
        exists: None,
    },
    Fk {
        field: "next_log",
        ty: Ty::Uuid,
        owner: Some("SELECT user_id FROM manager_workoutlog WHERE id = $1"),
        exists: None,
    },
    Fk {
        field: "exercise",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM exercises_exercise WHERE id = $1"),
    },
    Fk {
        field: "repetitions_unit",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM core_repetitionunit WHERE id = $1"),
    },
    Fk {
        field: "weight_unit",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM core_weightunit WHERE id = $1"),
    },
];

const WORKOUTSESSION_FKS: &[Fk] = &[
    Fk {
        field: "routine",
        ty: Ty::I32,
        owner: Some("SELECT user_id FROM manager_routine WHERE id = $1"),
        exists: None,
    },
    Fk {
        field: "day",
        ty: Ty::I32,
        owner: Some(
            "SELECT r.user_id FROM manager_day d JOIN manager_routine r ON r.id = d.routine_id WHERE d.id = $1",
        ),
        exists: None,
    },
];

const MEAL_FKS: &[Fk] = &[Fk {
    field: "plan",
    ty: Ty::Uuid,
    owner: Some("SELECT user_id FROM nutrition_nutritionplan WHERE id = $1"),
    exists: None,
}];

const MEALITEM_FKS: &[Fk] = &[
    Fk {
        field: "meal",
        ty: Ty::Uuid,
        owner: Some(
            "SELECT p.user_id FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE m.id = $1",
        ),
        exists: None,
    },
    Fk {
        field: "ingredient",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM nutrition_ingredient WHERE id = $1"),
    },
    Fk {
        field: "weight_unit",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM core_weightunit WHERE id = $1"),
    },
];

const LOGITEM_FKS: &[Fk] = &[
    Fk {
        field: "plan",
        ty: Ty::Uuid,
        owner: Some("SELECT user_id FROM nutrition_nutritionplan WHERE id = $1"),
        exists: None,
    },
    Fk {
        field: "meal",
        ty: Ty::Uuid,
        owner: Some(
            "SELECT p.user_id FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE m.id = $1",
        ),
        exists: None,
    },
    Fk {
        field: "ingredient",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM nutrition_ingredient WHERE id = $1"),
    },
    Fk {
        field: "weight_unit",
        ty: Ty::I32,
        owner: None,
        exists: Some("SELECT 1 FROM core_weightunit WHERE id = $1"),
    },
];

const PROFILE_DROP: &[&str] = &[
    "username",
    "email",
    "email_verified",
    "is_trustworthy",
    "date_joined",
    "gym",
    "is_temporary",
];

const TABLES: &[TableSpec] = &[
    TableSpec {
        table: "measurements_category",
        label: "Category",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: Some("user_id"),
        cols: CATEGORY_COLS,
        fks: CATEGORY_FKS,
        owned_select: "SELECT is_official, metric_type, dynamic_type FROM measurements_category WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &[],
    },
    TableSpec {
        table: "measurements_measurement",
        label: "Measurement",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: None,
        cols: MEASUREMENT_COLS,
        fks: MEASUREMENT_FKS,
        owned_select: "SELECT m.source FROM measurements_measurement m JOIN measurements_category c ON c.id = m.category_id WHERE m.id = $1 AND c.user_id = $2",
        owned: Owned::Via("category_id", "SELECT id FROM measurements_category WHERE user_id = "),
        drop: &[],
    },
    TableSpec {
        table: "core_userprofile",
        label: "UserProfile",
        pk: Pk::I32,
        create: false,
        delete: false,
        force_user: None,
        cols: PROFILE_COLS,
        fks: PROFILE_FKS,
        owned_select: "SELECT 1 FROM core_userprofile WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: PROFILE_DROP,
    },
    TableSpec {
        table: "manager_workoutlog",
        label: "WorkoutLog",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: Some("user_id"),
        cols: WORKOUTLOG_COLS,
        fks: WORKOUTLOG_FKS,
        owned_select: "SELECT 1 FROM manager_workoutlog WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &[],
    },
    TableSpec {
        table: "manager_workoutsession",
        label: "WorkoutSession",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: Some("user_id"),
        cols: WORKOUTSESSION_COLS,
        fks: WORKOUTSESSION_FKS,
        owned_select: "SELECT 1 FROM manager_workoutsession WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &[],
    },
    TableSpec {
        table: "manager_routine",
        label: "Routine",
        pk: Pk::I32,
        create: false,
        delete: true,
        force_user: Some("user_id"),
        cols: ROUTINE_COLS,
        fks: &[],
        owned_select: "SELECT start, \"end\" FROM manager_routine WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &[],
    },
    TableSpec {
        table: "gallery_image",
        label: "Image",
        pk: Pk::I32,
        create: false,
        delete: true,
        force_user: Some("user_id"),
        cols: GALLERY_COLS,
        fks: &[],
        owned_select: "SELECT 1 FROM gallery_image WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &["image"],
    },
    TableSpec {
        table: "nutrition_nutritionplan",
        label: "NutritionPlan",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: Some("user_id"),
        cols: PLAN_COLS,
        fks: &[],
        owned_select: "SELECT 1 FROM nutrition_nutritionplan WHERE id = $1 AND user_id = $2",
        owned: Owned::Direct,
        drop: &[],
    },
    TableSpec {
        table: "nutrition_meal",
        label: "Meal",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: None,
        cols: MEAL_COLS,
        fks: MEAL_FKS,
        owned_select: "SELECT 1 FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE m.id = $1 AND p.user_id = $2",
        owned: Owned::Via("plan_id", "SELECT id FROM nutrition_nutritionplan WHERE user_id = "),
        drop: &[],
    },
    TableSpec {
        table: "nutrition_mealitem",
        label: "MealItem",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: None,
        cols: MEALITEM_COLS,
        fks: MEALITEM_FKS,
        owned_select: "SELECT 1 FROM nutrition_mealitem mi JOIN nutrition_meal m ON m.id = mi.meal_id JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE mi.id = $1 AND p.user_id = $2",
        owned: Owned::Via("meal_id", "SELECT m.id FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE p.user_id = "),
        drop: &[],
    },
    TableSpec {
        table: "nutrition_logitem",
        label: "LogItem",
        pk: Pk::Uuid,
        create: true,
        delete: true,
        force_user: None,
        cols: LOGITEM_COLS,
        fks: LOGITEM_FKS,
        owned_select: "SELECT 1 FROM nutrition_logitem li JOIN nutrition_nutritionplan p ON p.id = li.plan_id WHERE li.id = $1 AND p.user_id = $2",
        owned: Owned::Via("plan_id", "SELECT id FROM nutrition_nutritionplan WHERE user_id = "),
        drop: &[],
    },
];

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let h = upload;
    let method = put(h).patch(h).delete(h);
    vec![
        ("/api/v2/upload-powersync-data", method.clone()),
        ("/api/v2/upload-powersync-data/", method),
    ]
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    Json(body): Json<Value>,
) -> Response {
    let user = match httpx::require_user(&state, &headers).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };
    let (table, data) = match extract(&body) {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };
    let Some(spec) = TABLES.iter().find(|t| t.table == table) else {
        return ok_json(json!({"error": format!("Unknown table: {table}")}));
    };
    let payload = match data {
        Value::Object(map) => map,
        _ => {
            return ok_json(json!({
                "error": "Validation failed",
                "details": "Invalid data. Expected a dictionary.",
            }))
        }
    };
    match method.as_str() {
        "PUT" => handle_create(&state, user.id, spec, payload).await,
        "PATCH" => handle_update(&state, user.id, spec, payload).await,
        "DELETE" => handle_delete(&state, user.id, spec, payload).await,
        other => ok_json(json!({
            "error": "Method not allowed",
            "details": format!("Unsupported HTTP verb: {other}"),
        })),
    }
}

fn extract(body: &Value) -> Result<(&str, &Value), Response> {
    let Value::Object(map) = body else {
        return Err(missing_fields());
    };
    let table = map
        .get("table")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match (table, map.get("data")) {
        (Some(table), Some(data)) => Ok((table, data)),
        _ => Err(missing_fields()),
    }
}

fn missing_fields() -> Response {
    ok_json(json!({"error": "Missing required fields: table, data"}))
}

fn ok_status() -> Response {
    ok_json(json!({"status": "ok!"}))
}

fn forbidden(details: &str) -> Response {
    ok_json(json!({"error": "Forbidden", "details": details}))
}

fn validation(details: Value) -> Response {
    ok_json(json!({"error": "Validation failed", "details": details}))
}

fn field_error(field: &str, message: &str) -> Response {
    validation(json!({field: [message]}))
}

fn method_not_allowed(spec: &TableSpec, action: &str) -> Response {
    let details = match action {
        "create" => format!("{} creation must go through the REST API", spec.label),
        _ => format!("{} cannot be deleted", spec.label),
    };
    ok_json(json!({"error": "Method not allowed", "details": details}))
}

fn transient() -> Response {
    httpx::json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error": "Temporarily unavailable"}),
    )
}

fn db_response(err: sqlx::Error) -> Response {
    match &err {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some(code) if code.starts_with("23") || code.starts_with("22") => {
                validation(Value::String(db.message().to_string()))
            }
            _ => {
                tracing::error!(error = %err, "upload transient db");
                transient()
            }
        },
        sqlx::Error::Io(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::Protocol(_) => {
            tracing::error!(error = %err, "upload transient db");
            transient()
        }
        _ => {
            tracing::error!(error = %err, "upload db");
            httpx::json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": err.to_string()}),
            )
        }
    }
}

#[derive(Clone)]
enum Bind {
    Uuid(Uuid),
    I32(i32),
    Dec(Decimal),
    Bool(bool),
    Str(String),
    Json(Value),
    Date(chrono::NaiveDate),
    Ts(chrono::DateTime<chrono::Utc>),
    Time(chrono::NaiveTime),
    Null,
}

enum Cell {
    Bind(Bind),
    /// SQL literal from the static column registry (never user input).
    Raw(String),
}

fn push_bind(q: &mut QueryBuilder<'_, sqlx::Postgres>, bind: Bind) {
    match bind {
        Bind::Uuid(v) => {
            q.push_bind(v);
        }
        Bind::I32(v) => {
            q.push_bind(v);
        }
        Bind::Dec(v) => {
            q.push_bind(v);
        }
        Bind::Bool(v) => {
            q.push_bind(v);
        }
        Bind::Str(v) => {
            q.push_bind(v);
        }
        Bind::Json(v) => {
            q.push_bind(v);
        }
        Bind::Date(v) => {
            q.push_bind(v);
        }
        Bind::Ts(v) => {
            q.push_bind(v);
        }
        Bind::Time(v) => {
            q.push_bind(v);
        }
        Bind::Null => {
            q.push("NULL");
        }
    }
}

fn coerce(ty: Ty, field: &str, value: &Value) -> Result<Bind, Response> {
    match ty {
        Ty::Uuid => match value {
            Value::String(s) => Uuid::parse_str(s)
                .map(Bind::Uuid)
                .map_err(|_| field_error(field, "Must be a valid UUID.")),
            _ => Err(field_error(field, "Must be a valid UUID.")),
        },
        Ty::I32 => match value {
            Value::Number(n) => n
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .map(Bind::I32)
                .ok_or_else(|| field_error(field, "Must be an integer.")),
            Value::String(s) => s
                .trim()
                .parse::<i32>()
                .map(Bind::I32)
                .map_err(|_| field_error(field, "Must be an integer.")),
            _ => Err(field_error(field, "Must be an integer.")),
        },
        Ty::Dec => number_dec(value, field).map(Bind::Dec),
        Ty::Bool => match value {
            Value::Bool(b) => Ok(Bind::Bool(*b)),
            Value::Number(n) => match n.as_i64() {
                Some(0) => Ok(Bind::Bool(false)),
                Some(1) => Ok(Bind::Bool(true)),
                _ => Err(field_error(field, "Must be a valid boolean.")),
            },
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Ok(Bind::Bool(true)),
                "false" | "f" | "0" => Ok(Bind::Bool(false)),
                _ => Err(field_error(field, "Must be a valid boolean.")),
            },
            _ => Err(field_error(field, "Must be a valid boolean.")),
        },
        Ty::Str => match value {
            Value::String(s) => Ok(Bind::Str(s.clone())),
            _ => Err(field_error(field, "Not a valid string.")),
        },
        Ty::Date => match value {
            Value::String(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(Bind::Date)
                .map_err(|_| {
                    field_error(
                        field,
                        "Date has wrong format. Use one of these formats instead: YYYY-MM-DD.",
                    )
                }),
            _ => Err(field_error(
                field,
                "Date has wrong format. Use one of these formats instead: YYYY-MM-DD.",
            )),
        },
        Ty::Ts => match value {
            Value::String(s) if !s.is_empty() => parse_ts(s)
                .map(Bind::Ts)
                .map_err(|_| {
                    field_error(
                        field,
                        "Datetime has wrong format. Use one of these formats instead: YYYY-MM-DD[ |T]HH:MM[:SS[.ffffff]][+HH:MM | Z].",
                    )
                }),
            _ => Err(field_error(
                field,
                "Datetime has wrong format. Use one of these formats instead: YYYY-MM-DD[ |T]HH:MM[:SS[.ffffff]][+HH:MM | Z].",
            )),
        },
        Ty::Time => match value {
            Value::String(s) => NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
                .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
                .map(Bind::Time)
                .map_err(|_| {
                    field_error(field, "Time has wrong format. Use one of these formats instead: HH:MM[:SS].")
                }),
            _ => Err(field_error(
                field,
                "Time has wrong format. Use one of these formats instead: HH:MM[:SS].",
            )),
        },
        Ty::Json => match value {
            Value::Object(_) => Ok(Bind::Json(value.clone())),
            Value::String(s) if s.is_empty() => Ok(Bind::Json(json!({}))),
            Value::String(s) => serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_object)
                .map(Bind::Json)
                .ok_or_else(|| field_error(field, &format!("{field} must be an object"))),
            _ => Err(field_error(field, &format!("{field} must be an object"))),
        },
    }
}

fn parse_ts(s: &str) -> Result<chrono::DateTime<chrono::Utc>, ()> {
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(ts.with_timezone(&chrono::Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(naive.and_utc());
        }
        if fmt == "%Y-%m-%d" {
            if let Ok(date) = NaiveDate::parse_from_str(s, fmt) {
                return Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc());
            }
        }
    }
    Err(())
}

fn number_dec(value: &Value, field: &str) -> Result<Decimal, Response> {
    match value {
        Value::Number(n) => Decimal::from_str_exact(n.to_string().as_str())
            .or_else(|_| n.as_f64().and_then(Decimal::from_f64_retain).ok_or(()))
            .map_err(|_| field_error(field, "Must be a number.")),
        Value::String(s) => s
            .trim()
            .parse::<Decimal>()
            .map_err(|_| field_error(field, "Must be a number.")),
        _ => Err(field_error(field, "Must be a number.")),
    }
}

fn decode_json(spec: &TableSpec, payload: &Map<String, Value>) -> Map<String, Value> {
    let mut out = payload.clone();
    for col in spec.cols.iter().filter(|c| c.ty == Ty::Json) {
        if let Some(Value::String(s)) = out.get(col.field) {
            let decoded = if s.is_empty() {
                Some(json!({}))
            } else {
                serde_json::from_str::<Value>(s)
                    .ok()
                    .filter(Value::is_object)
            };
            if let Some(v) = decoded {
                out.insert(col.field.to_string(), v);
            }
        }
    }
    out
}

fn parse_pk(spec: &TableSpec, value: &Value) -> Result<Bind, Response> {
    coerce(
        match spec.pk {
            Pk::Uuid => Ty::Uuid,
            Pk::I32 => Ty::I32,
        },
        "id",
        value,
    )
}

fn push_pk(q: &mut QueryBuilder<'_, sqlx::Postgres>, id: &Bind) {
    match id {
        Bind::Uuid(v) => {
            q.push_bind(*v);
        }
        Bind::I32(v) => {
            q.push_bind(*v);
        }
        _ => unreachable!("pk bind is uuid or i32"),
    }
}

async fn owned_row(
    pool: &PgPool,
    spec: &TableSpec,
    id: &Bind,
    user_id: i32,
) -> Result<Option<sqlx::postgres::PgRow>, Response> {
    let result = match id {
        Bind::Uuid(v) => {
            sqlx::query(spec.owned_select)
                .bind(*v)
                .bind(user_id)
                .fetch_optional(pool)
                .await
        }
        Bind::I32(v) => {
            sqlx::query(spec.owned_select)
                .bind(*v)
                .bind(user_id)
                .fetch_optional(pool)
                .await
        }
        _ => unreachable!("pk bind is uuid or i32"),
    };
    result.map_err(db_response)
}

fn push_owned_where(
    q: &mut QueryBuilder<'_, sqlx::Postgres>,
    spec: &TableSpec,
    id: &Bind,
    user_id: i32,
) {
    match &spec.owned {
        Owned::Direct => {
            q.push("id = ");
            push_pk(q, id);
            q.push(" AND user_id = ");
            q.push_bind(user_id);
        }
        Owned::Via(fk, sub) => {
            q.push("id = ");
            push_pk(q, id);
            q.push(format!(" AND \"{fk}\""));
            q.push(" IN (");
            q.push(sub);
            q.push_bind(user_id);
            q.push(")");
        }
    }
}

async fn check_fks(
    pool: &PgPool,
    spec: &TableSpec,
    payload: &Map<String, Value>,
    user_id: i32,
) -> Result<(), Response> {
    for fk in spec.fks {
        let Some(value) = payload.get(fk.field) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        // A malformed pk is skipped here and rejected by field validation below.
        let Ok(bind) = coerce(fk.ty, fk.field, value) else {
            continue;
        };
        if let Some(sql) = fk.owner {
            let row = match &bind {
                Bind::Uuid(v) => sqlx::query(sql).bind(*v).fetch_optional(pool).await,
                Bind::I32(v) => sqlx::query(sql).bind(*v).fetch_optional(pool).await,
                _ => unreachable!("fk bind is uuid or i32"),
            }
            .map_err(db_response)?;
            let owned = match row {
                Some(row) => {
                    let owner: i32 = row.get(0);
                    owner == user_id
                }
                None => false,
            };
            if !owned {
                return Err(forbidden(&format!(
                    "{} references an object you do not own",
                    spec.label
                )));
            }
        } else if let Some(sql) = fk.exists {
            let row = match &bind {
                Bind::Uuid(v) => sqlx::query(sql).bind(*v).fetch_optional(pool).await,
                Bind::I32(v) => sqlx::query(sql).bind(*v).fetch_optional(pool).await,
                _ => unreachable!("fk bind is uuid or i32"),
            }
            .map_err(db_response)?;
            if row.is_none() {
                let shown = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Err(field_error(
                    fk.field,
                    &format!("Invalid pk \"{shown}\" - object does not exist."),
                ));
            }
        }
    }
    Ok(())
}

async fn handle_create(
    state: &AppState,
    user_id: i32,
    spec: &TableSpec,
    payload: &Map<String, Value>,
) -> Response {
    if !spec.create {
        return method_not_allowed(spec, "create");
    }
    let mut data = decode_json(spec, payload);
    for key in spec.drop {
        data.remove(*key);
    }
    if let Some(id) = data.get("id") {
        match parse_pk(spec, id) {
            Ok(bind) => match owned_row(&state.pool, spec, &bind, user_id).await {
                Ok(Some(_)) => return ok_status(),
                Ok(None) => {}
                Err(resp) => return resp,
            },
            Err(resp) => return resp,
        }
    }
    if let Err(resp) = check_fks(&state.pool, spec, &data, user_id).await {
        return resp;
    }
    if let Err(resp) = create_side_checks(&state.pool, user_id, spec, &mut data).await {
        return resp;
    }
    if spec.table == "manager_workoutlog" {
        if let Err(resp) = workoutlog_clean(&data) {
            return resp;
        }
        if let Err(resp) = prepare_workoutlog(&state.pool, user_id, &mut data).await {
            return resp;
        }
    }
    let mut cells: Vec<(String, Cell)> = Vec::new();
    for col in spec.cols.iter().filter(|c| c.on_create) {
        if col.readonly {
            cells.push((
                col.column.to_string(),
                Cell::Raw(col.default.unwrap().to_string()),
            ));
            continue;
        }
        match data.get(col.field) {
            Some(value) if value.is_null() => {
                if !col.nullable {
                    return field_error(col.field, "This field may not be null.");
                }
                cells.push((col.column.to_string(), Cell::Bind(Bind::Null)));
            }
            Some(value) => match coerce(col.ty, col.field, value) {
                Ok(bind) => cells.push((col.column.to_string(), Cell::Bind(bind))),
                Err(resp) => return resp,
            },
            None => {
                if let Some(default) = col.default {
                    cells.push((col.column.to_string(), Cell::Raw(default.to_string())));
                } else if !col.nullable {
                    return field_error(col.field, "This field is required.");
                }
            }
        }
    }
    if let Some(user_col) = spec.force_user {
        cells.push((user_col.to_string(), Cell::Bind(Bind::I32(user_id))));
    }
    let mut q = QueryBuilder::new(format!("INSERT INTO {} (", spec.table));
    for (i, (column, _)) in cells.iter().enumerate() {
        if i > 0 {
            q.push(", ");
        }
        q.push(format!("\"{column}\""));
    }
    q.push(") VALUES (");
    for (i, (_, cell)) in cells.iter().enumerate() {
        if i > 0 {
            q.push(", ");
        }
        match cell {
            Cell::Bind(bind) => push_bind(&mut q, bind.clone()),
            Cell::Raw(expr) => {
                q.push(expr.as_str());
            }
        }
    }
    q.push(')');
    match q.build().execute(&state.pool).await {
        Ok(_) => {
            if spec.table == "manager_workoutlog" {
                if let Err(err) = backfill_session_day(&state.pool, &data).await {
                    return db_response(err);
                }
            }
            ok_status()
        }
        Err(err) => db_response(err),
    }
}

/// `WorkoutLog.clean()`: weight/reps presence and their units.
fn workoutlog_clean(data: &Map<String, Value>) -> Result<(), Response> {
    // absent units take the model defaults (repetitions / kg); explicit null stays null
    let present = |k: &str| match data.get(k) {
        Some(v) => !v.is_null(),
        None => k == "repetitions_unit" || k == "weight_unit",
    };
    let msg = if !present("repetitions") && !present("weight") {
        Some("Both repetitions and weight cannot be null at the same time.")
    } else if present("repetitions") && !present("repetitions_unit") {
        Some("Repetitions unit must be present if repetitions have a value.")
    } else if present("weight") && !present("weight_unit") {
        Some("Weight unit must be present if weight has a value.")
    } else {
        None
    };
    match msg {
        Some(m) => Err(validation(json!({"non_field_errors": [m]}))),
        None => Ok(()),
    }
}

/// Max session length (`WGER_MAX_SESSION_LENGTH_HOURS`, default 5).
fn max_session_hours() -> i32 {
    std::env::var("WGER_MAX_SESSION_LENGTH_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

fn json_i32(v: Option<&Value>) -> Option<i32> {
    match v {
        Some(Value::Number(n)) => n.as_i64().map(|x| x as i32),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// `WorkoutLog.save()` + `assign_session()`: drop a foreign session, then
/// attach to a covering session, else the latest open session that started
/// within the max session length before the log, else create one.
pub(crate) async fn prepare_workoutlog(
    pool: &PgPool,
    user_id: i32,
    data: &mut Map<String, Value>,
) -> Result<(), Response> {
    if let Some(Value::String(sid)) = data.get("session").cloned() {
        if let Ok(sid) = Uuid::parse_str(&sid) {
            let owner: Option<i32> =
                sqlx::query_scalar("SELECT user_id FROM manager_workoutsession WHERE id = $1")
                    .bind(sid)
                    .fetch_optional(pool)
                    .await
                    .map_err(db_response)?;
            if owner == Some(user_id) {
                return Ok(());
            }
        }
        data.insert("session".into(), Value::Null);
    }
    let Some(date_raw) = data.get("date").and_then(Value::as_str).map(str::to_string) else {
        return Ok(());
    };
    let Ok(log_date) = parse_ts(&date_raw) else {
        return Ok(());
    };
    let routine = json_i32(data.get("routine"));
    let slot_entry = json_i32(data.get("slot_entry"));
    let mut tx = pool.begin().await.map_err(db_response)?;
    sqlx::query("SELECT id FROM auth_user WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(db_response)?;
    let covering: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM manager_workoutsession WHERE user_id = $1 AND routine_id IS NOT DISTINCT FROM $2          AND datetime_start <= $3 AND datetime_end >= $3 ORDER BY datetime_start LIMIT 1",
    )
    .bind(user_id)
    .bind(routine)
    .bind(log_date)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_response)?;
    let session = match covering {
        Some(id) => Some(id),
        None => sqlx::query_scalar(
            "SELECT id FROM manager_workoutsession WHERE user_id = $1 AND routine_id IS NOT DISTINCT FROM $2              AND datetime_end IS NULL AND datetime_start >= $3 - make_interval(hours => $4)              AND datetime_start <= $3 ORDER BY datetime_start DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(routine)
        .bind(log_date)
        .bind(max_session_hours())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_response)?,
    };
    let session = match session {
        Some(id) => id,
        None => {
            let mut day: Option<i32> = None;
            if let (Some(entry), Some(r)) = (slot_entry, routine) {
                day = sqlx::query_scalar(
                    "SELECT d.id FROM manager_slotentry e JOIN manager_slot s ON s.id = e.slot_id                      JOIN manager_day d ON d.id = s.day_id WHERE e.id = $1 AND d.routine_id = $2",
                )
                .bind(entry)
                .bind(r)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_response)?;
            }
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO manager_workoutsession (id, user_id, routine_id, day_id, datetime_start, notes, impression)                  VALUES ($1, $2, $3, $4, $5, NULL, '2')",
            )
            .bind(id)
            .bind(user_id)
            .bind(routine)
            .bind(day)
            .bind(log_date)
            .execute(&mut *tx)
            .await
            .map_err(db_response)?;
            id
        }
    };
    tx.commit().await.map_err(db_response)?;
    data.insert("session".into(), json!(session.to_string()));
    Ok(())
}

/// Fill an empty session day when all its slot-entry logs agree on one day
/// of the session routine (WorkoutLog.save tail).
pub(crate) async fn backfill_session_day(
    pool: &PgPool,
    data: &Map<String, Value>,
) -> Result<(), sqlx::Error> {
    let Some(sid) = data
        .get("session")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Ok(());
    };
    if data.get("slot_entry").map(|v| v.is_null()).unwrap_or(true) {
        return Ok(());
    }
    sqlx::query(
        "UPDATE manager_workoutsession ws SET day_id = x.day_id FROM (            SELECT MIN(d.id) AS day_id, MIN(d.routine_id) AS routine_id, COUNT(DISTINCT d.id) AS n            FROM manager_workoutlog l JOIN manager_slotentry e ON e.id = l.slot_entry_id            JOIN manager_slot s ON s.id = e.slot_id JOIN manager_day d ON d.id = s.day_id            WHERE l.session_id = $1) x          WHERE ws.id = $1 AND ws.day_id IS NULL AND x.n = 1 AND ws.routine_id = x.routine_id",
    )
    .bind(sid)
    .execute(pool)
    .await?;
    Ok(())
}

async fn create_side_checks(
    pool: &sqlx::PgPool,
    user_id: i32,
    spec: &TableSpec,
    data: &mut Map<String, Value>,
) -> Result<(), Response> {
    match spec.table {
        "measurements_category" => {
            let metric = data
                .get("metric_type")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(metric) = metric {
                if metric != "custom" && metric != "body_weight" {
                    let derived = Uuid::new_v5(
                        &CATEGORY_NAMESPACE,
                        format!("{user_id}:{metric}").as_bytes(),
                    );
                    data.insert("id".to_string(), json!(derived));
                }
                if metric == "body_weight" {
                    let unit = data.get("unit").and_then(Value::as_str).unwrap_or("");
                    if unit != "kg" && unit != "lb" {
                        return Err(field_error(
                            "unit",
                            "Body weight categories only support kg and lb as unit",
                        ));
                    }
                }
            }
        }
        "measurements_measurement" => {
            // Same serializer as REST in Django: stamp body weight unit.
            let cat = data
                .get("category")
                .and_then(Value::as_str)
                .and_then(|c| uuid::Uuid::parse_str(c).ok());
            if let Some(cat) = cat {
                let row: Option<(String, String)> = sqlx::query_as(
                    "SELECT metric_type, unit FROM measurements_category WHERE id = $1",
                )
                .bind(cat)
                .fetch_optional(pool)
                .await
                .map_err(httpx::db_error)?;
                if let Some((mt, unit)) = row {
                    if mt == "body_weight" {
                        let mut ed = match data.get("extra_data") {
                            Some(Value::Object(m)) => m.clone(),
                            Some(Value::String(t)) => match serde_json::from_str::<Value>(t) {
                                Ok(Value::Object(m)) => m,
                                _ => serde_json::Map::new(),
                            },
                            _ => serde_json::Map::new(),
                        };
                        ed.entry("unit".to_string()).or_insert(json!(unit));
                        data.insert("extra_data".to_string(), Value::Object(ed));
                    }
                }
            }
            if let Some(value) = data.get("value") {
                if let Ok(Bind::Dec(d)) = coerce(Ty::Dec, "value", value) {
                    if d.is_sign_negative() {
                        return Err(field_error(
                            "value",
                            "Ensure this value is greater than or equal to 0.",
                        ));
                    }
                }
            }
        }
        "manager_workoutsession" => {
            if let Some(Value::String(impression)) = data.get("impression") {
                if !IMPRESSIONS.contains(&impression.as_str()) {
                    return Err(field_error(
                        "impression",
                        &format!("\"{impression}\" is not a valid choice."),
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_update(
    state: &AppState,
    user_id: i32,
    spec: &TableSpec,
    payload: &Map<String, Value>,
) -> Response {
    let Some(id_value) = payload.get("id") else {
        return httpx::json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "'id'"}));
    };
    let id = match parse_pk(spec, id_value) {
        Ok(bind) => bind,
        Err(resp) => return resp,
    };
    let row = match owned_row(&state.pool, spec, &id, user_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return ok_status(),
        Err(resp) => return resp,
    };
    let mut data = decode_json(spec, payload);
    for key in spec.drop {
        data.remove(*key);
    }
    match spec.table {
        "measurements_category" => {
            let existing_metric: String = row.get("metric_type");
            if let Some(Value::String(metric)) = data.get("metric_type") {
                if *metric != existing_metric {
                    return field_error(
                        "metric_type",
                        "The metric type of a category cannot be changed",
                    );
                }
            }
            let existing_dynamic: String = row.get("dynamic_type");
            if let Some(Value::String(dynamic)) = data.get("dynamic_type") {
                if !existing_dynamic.eq_ignore_ascii_case("none") && *dynamic != existing_dynamic {
                    return field_error(
                        "dynamic_type",
                        "The calculation of a category cannot be changed",
                    );
                }
            }
        }
        "measurements_measurement" => {
            let source: String = row.get("source");
            if source == "calculated" {
                return validation(
                    json!({"non_field_errors": ["A calculated entry is maintained by the server"]}),
                );
            }
            if let Some(value) = data.get("value") {
                if let Ok(Bind::Dec(d)) = coerce(Ty::Dec, "value", value) {
                    if d.is_sign_negative() {
                        return field_error(
                            "value",
                            "Ensure this value is greater than or equal to 0.",
                        );
                    }
                }
            }
        }
        "manager_workoutsession" => {
            if let Some(Value::String(impression)) = data.get("impression") {
                if !IMPRESSIONS.contains(&impression.as_str()) {
                    return field_error(
                        "impression",
                        &format!("\"{impression}\" is not a valid choice."),
                    );
                }
            }
        }
        "manager_routine" => {
            if let (Some(start), Some(end)) = (data.get("start"), data.get("end")) {
                if let (Some(s), Some(e)) = (
                    start
                        .as_str()
                        .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()),
                    end.as_str()
                        .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()),
                ) {
                    if e < s {
                        return field_error("end", "The end date cannot be before the start date.");
                    }
                }
            }
        }
        _ => {}
    }
    if let Err(resp) = check_fks(&state.pool, spec, &data, user_id).await {
        return resp;
    }
    let mut sets: Vec<(String, Bind)> = Vec::new();
    for col in spec.cols.iter().filter(|c| c.on_update) {
        let Some(value) = data.get(col.field) else {
            continue;
        };
        if value.is_null() {
            if !col.nullable {
                return field_error(col.field, "This field may not be null.");
            }
            sets.push((col.column.to_string(), Bind::Null));
            continue;
        }
        match coerce(col.ty, col.field, value) {
            Ok(bind) => sets.push((col.column.to_string(), bind)),
            Err(resp) => return resp,
        }
    }
    if sets.is_empty() {
        return ok_status();
    }
    let mut q = QueryBuilder::new(format!("UPDATE {} SET ", spec.table));
    for (i, (column, bind)) in sets.iter().enumerate() {
        if i > 0 {
            q.push(", ");
        }
        q.push(format!("\"{column}\""));
        q.push(" = ");
        push_bind(&mut q, bind.clone());
    }
    q.push(" WHERE ");
    push_owned_where(&mut q, spec, &id, user_id);
    match q.build().execute(&state.pool).await {
        Ok(_) => ok_status(),
        Err(err) => db_response(err),
    }
}

async fn handle_delete(
    state: &AppState,
    user_id: i32,
    spec: &TableSpec,
    payload: &Map<String, Value>,
) -> Response {
    if !spec.delete {
        return method_not_allowed(spec, "delete");
    }
    let Some(id_value) = payload.get("id") else {
        return httpx::json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "'id'"}));
    };
    let id = match parse_pk(spec, id_value) {
        Ok(bind) => bind,
        Err(resp) => return resp,
    };
    let row = match owned_row(&state.pool, spec, &id, user_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return ok_status(),
        Err(resp) => return resp,
    };
    match spec.table {
        "measurements_category" => {
            let is_official: bool = row.get("is_official");
            if is_official {
                return forbidden("Official categories cannot be deleted");
            }
        }
        "measurements_measurement" => {
            let source: String = row.get("source");
            if source == "calculated" {
                return forbidden("A calculated entry is maintained by the server");
            }
        }
        _ => {}
    }
    let mut q = QueryBuilder::new(format!("DELETE FROM {} WHERE ", spec.table));
    push_owned_where(&mut q, spec, &id, user_id);
    if spec.table == "gallery_image" {
        q.push(" RETURNING image");
        return match q.build().fetch_optional(&state.pool).await {
            Ok(Some(r)) => {
                let image: String = r.get("image");
                remove_media_file(&state.config.media_root, &image).await;
                ok_status()
            }
            Ok(None) => ok_status(),
            Err(err) => db_response(err),
        };
    }
    match q.build().execute(&state.pool).await {
        Ok(_) => ok_status(),
        Err(err) => db_response(err),
    }
}

/// gallery `post_delete` signal: remove the stored file (path relative to
/// MEDIA_ROOT). Refuses absolute paths and `..`; a missing file is ignored.
pub(crate) async fn remove_media_file(root: &str, rel: &str) {
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|p| p == "..") {
        return;
    }
    let _ = tokio::fs::remove_file(std::path::Path::new(root).join(rel)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_sql(field: &str) -> &'static str {
        WORKOUTLOG_FKS
            .iter()
            .find(|fk| fk.field == field)
            .and_then(|fk| fk.owner)
            .unwrap_or_else(|| panic!("{field} must have an owner query"))
    }

    /// `manager_slot` has `day_id`, not `routine_id`: the workoutlog slot_entry
    /// owner check must climb slot -> day -> routine. Using `s.routine_id`
    /// raised Postgres 42703, which the upload handler maps to a transient 503,
    /// so PowerSync retried the row forever.
    #[test]
    fn workoutlog_slot_entry_owner_joins_through_day() {
        let sql = owner_sql("slot_entry");
        assert!(
            !sql.contains("s.routine_id"),
            "manager_slot has no routine_id column: {sql}"
        );
        assert!(
            !sql.contains("se.routine_id"),
            "manager_slotentry has no routine_id column: {sql}"
        );
        assert!(
            sql.contains("JOIN manager_day d ON d.id = s.day_id"),
            "slot_entry owner must join manager_day through slot.day_id: {sql}"
        );
        assert!(
            sql.contains("JOIN manager_routine r ON r.id = d.routine_id"),
            "slot_entry owner must reach the routine through day.routine_id: {sql}"
        );
        assert!(
            sql.contains("WHERE se.id = $1"),
            "slot_entry owner must filter on the slot entry pk: {sql}"
        );
        assert!(
            sql.contains("SELECT r.user_id"),
            "slot_entry owner must select the routine owner: {sql}"
        );
    }
}
