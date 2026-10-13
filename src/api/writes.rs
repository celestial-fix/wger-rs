//! REST writes (DRF ModelViewSet semantics) for the routine tree, the 10 set
//! configs, workout logs, nutrition diary, measurements and weight entries.
//!
//! One spec per resource. Flow mirrors `WgerOwnerObjectModelViewSet`:
//! FK ownership check (403 `You are not allowed to do this`, also for a
//! missing owner FK) -> object lookup in the user's queryset (404) ->
//! serializer field validation (400 `{field: [msg]}`) -> object validation ->
//! save -> response rendered by the existing detail GET handler so shapes are
//! identical to reads. POST 201, PUT/PATCH 200, DELETE 204.

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, MethodRouter};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use crate::httpx::{self, CurrentUser};
use crate::state::AppState;

type Render = for<'a> fn(&'a AppState, &'a HeaderMap, String) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>>;

#[derive(Clone, Copy)]
pub(crate) enum Ty {
    /// PositiveIntegerField / IntegerField: (min, max)
    Int(i64, i64),
    /// DecimalField: (max_digits, decimal_places, min, max)
    Dec(u32, u32, Option<i64>, Option<i64>),
    /// CharField(max_length, allow_blank)
    Str(usize, bool),
    Bool,
    Date,
    Ts,
    Json,
    Choice(&'static [&'static str]),
    /// FK with int pk. (exists sql `$1`, owner sql returning user_id or None
    /// when not an ownership-checked field)
    FkInt(&'static str, Option<&'static str>),
    FkUuid(&'static str, Option<&'static str>),
}

#[derive(Clone, Copy)]
pub(crate) struct F {
    name: &'static str,
    column: &'static str,
    ty: Ty,
    /// Required on create (no model default, not blank/null).
    required: bool,
    nullable: bool,
    /// SQL used on create when absent.
    default: Option<&'static str>,
}

const fn f(name: &'static str, column: &'static str, ty: Ty, required: bool, nullable: bool, default: Option<&'static str>) -> F {
    F { name, column, ty, required, nullable, default }
}

pub(crate) struct Res {
    path: &'static str,
    table: &'static str,
    uuid_pk: bool,
    fields: &'static [F],
    /// SQL condition on alias-less table; `$U` replaced by user id bind.
    owner: &'static str,
    force_user: bool,
    /// ModelViewSet allowed verbs on detail (PUT/PATCH/DELETE) and POST.
    update: bool,
    render: Render,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Routine,
    Config,
    SlotEntry,
    Log,
    Diary,
    Measurement,
    Weight,
}

const OPS: &[&str] = &["+", "-", "r"];
const STEPS: &[&str] = &["na", "abs", "percent"];
const DAY_TYPES: &[&str] = &["custom", "enom", "amrap", "hiit", "tabata", "edt", "rft", "afap"];
const ENTRY_TYPES: &[&str] = &["normal", "warmup", "dropset", "myo", "partial", "forced", "tut", "iso", "jump"];
const PINT: Ty = Ty::Int(0, 2147483647);

const OWN_ROUTINE: &str = "SELECT user_id FROM manager_routine WHERE id = $1";
const OWN_DAY: &str = "SELECT r.user_id FROM manager_day d JOIN manager_routine r ON r.id = d.routine_id WHERE d.id = $1";
const OWN_SLOT: &str = "SELECT r.user_id FROM manager_slot s JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id WHERE s.id = $1";
const OWN_ENTRY: &str = "SELECT r.user_id FROM manager_slotentry e JOIN manager_slot s ON s.id = e.slot_id JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id WHERE e.id = $1";
const OWN_SESSION: &str = "SELECT user_id FROM manager_workoutsession WHERE id = $1";
const OWN_LOG: &str = "SELECT user_id FROM manager_workoutlog WHERE id = $1";
const OWN_PLAN: &str = "SELECT user_id FROM nutrition_nutritionplan WHERE id = $1";
const OWN_MEAL: &str = "SELECT p.user_id FROM nutrition_meal m JOIN nutrition_nutritionplan p ON p.id = m.plan_id WHERE m.id = $1";
const OWN_CATEGORY: &str = "SELECT user_id FROM measurements_category WHERE id = $1";

const ROUTINE_FIELDS: &[F] = &[
    f("name", "name", Ty::Str(25, true), false, false, Some("''")),
    f("description", "description", Ty::Str(1000, true), false, false, Some("''")),
    f("start", "start", Ty::Date, true, false, None),
    f("end", "end", Ty::Date, true, false, None),
    f("fit_in_week", "fit_in_week", Ty::Bool, false, false, Some("false")),
    f("is_template", "is_template", Ty::Bool, false, false, Some("false")),
    f("is_public", "is_public", Ty::Bool, false, false, Some("false")),
];
const DAY_FIELDS: &[F] = &[
    f("routine", "routine_id", Ty::FkInt("SELECT 1 FROM manager_routine WHERE id = $1", Some(OWN_ROUTINE)), true, false, None),
    f("order", "order", PINT, false, false, Some("1")),
    f("name", "name", Ty::Str(20, true), false, false, Some("''")),
    f("description", "description", Ty::Str(1000, true), false, false, Some("''")),
    f("is_rest", "is_rest", Ty::Bool, false, false, Some("false")),
    f("need_logs_to_advance", "need_logs_to_advance", Ty::Bool, false, false, Some("false")),
    f("type", "type", Ty::Choice(DAY_TYPES), false, false, Some("'custom'")),
    f("config", "config", Ty::Json, false, true, Some("NULL")),
];
const SLOT_FIELDS: &[F] = &[
    f("day", "day_id", Ty::FkInt("SELECT 1 FROM manager_day WHERE id = $1", Some(OWN_DAY)), true, false, None),
    f("order", "order", PINT, false, false, Some("1")),
    f("comment", "comment", Ty::Str(200, true), false, false, Some("''")),
    f("config", "config", Ty::Json, false, true, Some("NULL")),
];
const ENTRY_FIELDS: &[F] = &[
    f("slot", "slot_id", Ty::FkInt("SELECT 1 FROM manager_slot WHERE id = $1", Some(OWN_SLOT)), true, false, None),
    f("exercise", "exercise_id", Ty::FkInt("SELECT 1 FROM exercises_exercise WHERE id = $1", None), true, false, None),
    f("type", "type", Ty::Choice(ENTRY_TYPES), false, false, Some("'normal'")),
    f("repetition_unit", "repetition_unit_id", Ty::FkInt("SELECT 1 FROM core_repetitionunit WHERE id = $1", None), false, true, Some("1")),
    f("repetition_rounding", "repetition_rounding", Ty::Dec(4, 2, None, None), false, true, Some("NULL")),
    f("weight_unit", "weight_unit_id", Ty::FkInt("SELECT 1 FROM core_weightunit WHERE id = $1", None), false, true, Some("1")),
    f("weight_rounding", "weight_rounding", Ty::Dec(4, 2, None, None), false, true, Some("NULL")),
    f("order", "order", PINT, false, false, None),
    f("comment", "comment", Ty::Str(100, true), false, false, Some("''")),
    f("config", "config", Ty::Json, false, true, Some("NULL")),
];
const CONFIG_FIELDS_DEC: &[F] = &[
    f("slot_entry", "slot_entry_id", Ty::FkInt("SELECT 1 FROM manager_slotentry WHERE id = $1", Some(OWN_ENTRY)), true, false, None),
    f("iteration", "iteration", PINT, true, false, None),
    f("value", "value", Ty::Dec(6, 2, Some(0), Some(3000)), true, false, None),
    f("operation", "operation", Ty::Choice(OPS), false, false, Some("'r'")),
    f("step", "step", Ty::Choice(STEPS), false, false, Some("'abs'")),
    f("repeat", "repeat", Ty::Bool, false, false, Some("false")),
    f("requirements", "requirements", Ty::Json, false, true, Some("NULL")),
];
const LOG_FIELDS: &[F] = &[
    f("date", "date", Ty::Ts, false, false, Some("now()")),
    f("session", "session_id", Ty::FkUuid("SELECT 1 FROM manager_workoutsession WHERE id = $1", Some(OWN_SESSION)), false, true, Some("NULL")),
    f("routine", "routine_id", Ty::FkInt("SELECT 1 FROM manager_routine WHERE id = $1", Some(OWN_ROUTINE)), false, true, Some("NULL")),
    f("iteration", "iteration", PINT, false, true, Some("NULL")),
    f("slot_entry", "slot_entry_id", Ty::FkInt("SELECT 1 FROM manager_slotentry WHERE id = $1", Some(OWN_ENTRY)), false, true, Some("NULL")),
    f("next_log", "next_log_id", Ty::FkUuid("SELECT 1 FROM manager_workoutlog WHERE id = $1", Some(OWN_LOG)), false, true, Some("NULL")),
    f("exercise", "exercise_id", Ty::FkInt("SELECT 1 FROM exercises_exercise WHERE id = $1", None), true, false, None),
    f("repetitions_unit", "repetitions_unit_id", Ty::FkInt("SELECT 1 FROM core_repetitionunit WHERE id = $1", None), false, true, Some("NULL")),
    f("repetitions", "repetitions", Ty::Dec(6, 2, Some(0), None), false, true, Some("NULL")),
    f("repetitions_target", "repetitions_target", Ty::Dec(6, 2, Some(0), None), false, true, Some("NULL")),
    f("weight_unit", "weight_unit_id", Ty::FkInt("SELECT 1 FROM core_weightunit WHERE id = $1", None), false, true, Some("NULL")),
    f("weight", "weight", Ty::Dec(6, 2, None, None), false, true, Some("NULL")),
    f("weight_target", "weight_target", Ty::Dec(6, 2, None, None), false, true, Some("NULL")),
    f("rir", "rir", Ty::Dec(2, 1, Some(0), None), false, true, Some("NULL")),
    f("rir_target", "rir_target", Ty::Dec(2, 1, Some(0), None), false, true, Some("NULL")),
    f("rest", "rest", PINT, false, true, Some("NULL")),
    f("rest_target", "rest_target", PINT, false, true, Some("NULL")),
];
const DIARY_FIELDS: &[F] = &[
    f("plan", "plan_id", Ty::FkUuid("SELECT 1 FROM nutrition_nutritionplan WHERE id = $1", Some(OWN_PLAN)), true, false, None),
    f("meal", "meal_id", Ty::FkUuid("SELECT 1 FROM nutrition_meal WHERE id = $1", Some(OWN_MEAL)), false, true, Some("NULL")),
    f("ingredient", "ingredient_id", Ty::FkInt("SELECT 1 FROM nutrition_ingredient WHERE id = $1", None), true, false, None),
    f("weight_unit", "weight_unit_id", Ty::FkInt("SELECT 1 FROM nutrition_ingredientweightunit WHERE id = $1", None), false, true, Some("NULL")),
    f("datetime", "datetime", Ty::Ts, false, false, Some("now()")),
    f("amount", "amount", Ty::Dec(6, 2, Some(0), Some(1000)), true, false, None),
];
const MEASUREMENT_FIELDS: &[F] = &[
    f("category", "category_id", Ty::FkUuid("SELECT 1 FROM measurements_category WHERE id = $1", Some(OWN_CATEGORY)), true, false, None),
    f("date", "date", Ty::Ts, true, false, None),
    f("value", "value", Ty::Dec(12, 4, Some(0), None), true, false, None),
    f("notes", "notes", Ty::Str(100, true), false, false, Some("''")),
    f("source", "source", Ty::Choice(&["manual", "import", "health", "calculated"]), false, false, Some("'manual'")),
    f("external_id", "external_id", Ty::FkUuid("SELECT 1", None), false, true, Some("NULL")),
    f("extra_data", "extra_data", Ty::Json, false, false, Some("'{}'::jsonb")),
];
const WEIGHT_FIELDS: &[F] = &[
    f("date", "date", Ty::Ts, true, false, None),
    f("weight", "value", Ty::Dec(12, 4, None, None), true, false, None),
];

const IN_OWN_ROUTINES: &str = "routine_id IN (SELECT id FROM manager_routine WHERE user_id = $U)";
const IN_OWN_DAYS: &str = "day_id IN (SELECT d.id FROM manager_day d JOIN manager_routine r ON r.id = d.routine_id WHERE r.user_id = $U)";
const IN_OWN_SLOTS: &str = "slot_id IN (SELECT s.id FROM manager_slot s JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id WHERE r.user_id = $U)";
const IN_OWN_ENTRIES: &str = "slot_entry_id IN (SELECT e.id FROM manager_slotentry e JOIN manager_slot s ON s.id = e.slot_id JOIN manager_day d ON d.id = s.day_id JOIN manager_routine r ON r.id = d.routine_id WHERE r.user_id = $U)";

macro_rules! render {
    ($name:ident, $call:expr) => {
        fn $name<'a>(state: &'a AppState, headers: &'a HeaderMap, id: String) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
            Box::pin(async move { ($call)(state, headers, id).await })
        }
    };
}

render!(r_routine, |s, h, id| super::manager::render_routine(s, h, id));
render!(r_day, |s, h, id| super::manager::render_owned(s, h, "day", id));
render!(r_slot, |s, h, id| super::manager::render_owned(s, h, "slot", id));
render!(r_entry, |s, h, id| super::manager::render_owned(s, h, "entry", id));
render!(r_log, |s, h, id| super::manager::render_owned(s, h, "log", id));
render!(r_diary, |s, h, id| super::nutrition::render_diary(s, h, id));
render!(r_measurement, |s, h, id| super::other::render_measurement(s, h, id));
render!(r_weight, |s, h, id| super::other::render_weight(s, h, id));

macro_rules! config_res {
    ($fname:ident, $path:literal, $table:literal) => {
        render!($fname, |s, h, id| super::manager::render_config(s, h, $table, id));
    };
}
config_res!(r_c_weight, "weight-config", "manager_weightconfig");
config_res!(r_c_maxweight, "max-weight-config", "manager_maxweightconfig");
config_res!(r_c_reps, "repetitions-config", "manager_repetitionsconfig");
config_res!(r_c_maxreps, "max-repetitions-config", "manager_maxrepetitionsconfig");
config_res!(r_c_sets, "sets-config", "manager_setsconfig");
config_res!(r_c_maxsets, "max-sets-config", "manager_maxsetsconfig");
config_res!(r_c_rest, "rest-config", "manager_restconfig");
config_res!(r_c_maxrest, "max-rest-config", "manager_maxrestconfig");
config_res!(r_c_rir, "rir-config", "manager_rirconfig");
config_res!(r_c_maxrir, "max-rir-config", "manager_maxrirconfig");

const fn cfg_fields(int_value: bool, rir: bool) -> &'static [F] {
    if int_value {
        CONFIG_FIELDS_INT
    } else if rir {
        CONFIG_FIELDS_DEC
    } else {
        CONFIG_FIELDS_DEC
    }
}
const CONFIG_FIELDS_INT: &[F] = CONFIG_FIELDS_DEC;

macro_rules! cfg {
    ($path:literal, $table:literal, $render:ident) => {
        Res { path: $path, table: $table, uuid_pk: false, fields: cfg_fields(false, false), owner: IN_OWN_ENTRIES, force_user: false, update: true, render: $render, kind: Kind::Config }
    };
}

pub(crate) static RESOURCES: &[Res] = &[
    Res { path: "routine", table: "manager_routine", uuid_pk: false, fields: ROUTINE_FIELDS, owner: "user_id = $U", force_user: true, update: true, render: r_routine, kind: Kind::Routine },
    Res { path: "day", table: "manager_day", uuid_pk: false, fields: DAY_FIELDS, owner: IN_OWN_ROUTINES, force_user: false, update: true, render: r_day, kind: Kind::Plain },
    Res { path: "slot", table: "manager_slot", uuid_pk: false, fields: SLOT_FIELDS, owner: IN_OWN_DAYS, force_user: false, update: true, render: r_slot, kind: Kind::Plain },
    Res { path: "slot-entry", table: "manager_slotentry", uuid_pk: false, fields: ENTRY_FIELDS, owner: IN_OWN_SLOTS, force_user: false, update: true, render: r_entry, kind: Kind::SlotEntry },
    cfg!("weight-config", "manager_weightconfig", r_c_weight),
    cfg!("max-weight-config", "manager_maxweightconfig", r_c_maxweight),
    cfg!("repetitions-config", "manager_repetitionsconfig", r_c_reps),
    cfg!("max-repetitions-config", "manager_maxrepetitionsconfig", r_c_maxreps),
    cfg!("sets-config", "manager_setsconfig", r_c_sets),
    cfg!("max-sets-config", "manager_maxsetsconfig", r_c_maxsets),
    cfg!("rest-config", "manager_restconfig", r_c_rest),
    cfg!("max-rest-config", "manager_maxrestconfig", r_c_maxrest),
    cfg!("rir-config", "manager_rirconfig", r_c_rir),
    cfg!("max-rir-config", "manager_maxrirconfig", r_c_maxrir),
    Res { path: "workoutlog", table: "manager_workoutlog", uuid_pk: true, fields: LOG_FIELDS, owner: "user_id = $U", force_user: true, update: true, render: r_log, kind: Kind::Log },
    Res { path: "nutritiondiary", table: "nutrition_logitem", uuid_pk: true, fields: DIARY_FIELDS, owner: "plan_id IN (SELECT id FROM nutrition_nutritionplan WHERE user_id = $U)", force_user: false, update: true, render: r_diary, kind: Kind::Diary },
    Res { path: "measurement", table: "measurements_measurement", uuid_pk: true, fields: MEASUREMENT_FIELDS, owner: "category_id IN (SELECT id FROM measurements_category WHERE user_id = $U)", force_user: false, update: true, render: r_measurement, kind: Kind::Measurement },
    Res { path: "weightentry", table: "measurements_measurement", uuid_pk: true, fields: WEIGHT_FIELDS, owner: "category_id IN (SELECT id FROM measurements_category WHERE user_id = $U AND is_official AND metric_type = 'body_weight')", force_user: false, update: true, render: r_weight, kind: Kind::Weight },
];

pub fn routes() -> Vec<(&'static str, MethodRouter<AppState>)> {
    let mut out = Vec::new();
    for (idx, _res) in RESOURCES.iter().enumerate() {
        let res = &RESOURCES[idx];
        let list = format!("/api/v2/{}", res.path);
        let detail = format!("/api/v2/{}/:id", res.path);
        for p in [list.clone(), format!("{list}/")] {
            let p: &'static str = Box::leak(p.into_boxed_str());
            out.push((p, post(move |State(st): State<AppState>, h: HeaderMap, body: Bytes| async move { create(&st, &h, &RESOURCES[idx], body).await })));
        }
        for p in [detail.clone(), format!("{detail}/")] {
            let p: &'static str = Box::leak(p.into_boxed_str());
            let r = axum::routing::patch(move |State(st): State<AppState>, h: HeaderMap, Path(id): Path<String>, body: Bytes| async move {
                update(&st, &h, &RESOURCES[idx], id, body, true).await
            })
            .put(move |State(st): State<AppState>, h: HeaderMap, Path(id): Path<String>, body: Bytes| async move {
                update(&st, &h, &RESOURCES[idx], id, body, false).await
            })
            .delete(move |State(st): State<AppState>, h: HeaderMap, Path(id): Path<String>| async move { destroy(&st, &h, &RESOURCES[idx], id).await });
            out.push((p, r));
        }
    }
    out
}

#[derive(Clone, Debug)]
enum B {
    Null,
    I(i64),
    D(Decimal),
    S(String),
    Bo(bool),
    Da(NaiveDate),
    Ts(DateTime<Utc>),
    J(Value),
    U(Uuid),
}

fn push_b(q: &mut QueryBuilder<'_, sqlx::Postgres>, b: B) {
    match b {
        B::Null => {
            q.push("NULL");
        }
        B::I(v) => {
            q.push_bind(v as i32);
        }
        B::D(v) => {
            q.push_bind(v);
        }
        B::S(v) => {
            q.push_bind(v);
        }
        B::Bo(v) => {
            q.push_bind(v);
        }
        B::Da(v) => {
            q.push_bind(v);
        }
        B::Ts(v) => {
            q.push_bind(v);
        }
        B::J(v) => {
            q.push_bind(sqlx::types::Json(v));
        }
        B::U(v) => {
            q.push_bind(v);
        }
    }
}

fn bad(errors: Map<String, Value>) -> Response {
    httpx::json_response(StatusCode::BAD_REQUEST, Value::Object(errors))
}
fn forbidden() -> Response {
    httpx::detail(StatusCode::FORBIDDEN, "You are not allowed to do this")
}

fn parse_body(body: &Bytes) -> Result<Map<String, Value>, Response> {
    if body.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(httpx::json_response(StatusCode::BAD_REQUEST, json!(["Request data is not a dictionary"]))),
        Err(e) => Err(httpx::json_response(
            StatusCode::BAD_REQUEST,
            json!({"detail": format!("JSON parse error - {e}")}),
        )),
    }
}

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Null => "NoneType",
    }
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => {
            let t = s.trim();
            let t = t.strip_suffix(".0").unwrap_or(t);
            t.parse().ok()
        }
        _ => None,
    }
}

fn dec_check(d: &Decimal, digits: u32, places: u32) -> Option<String> {
    let mant = d.mantissa().unsigned_abs();
    let scale = d.scale();
    let ndig = if mant == 0 { 1 } else { mant.to_string().len() as u32 };
    let total = ndig.max(scale);
    let whole = total - scale;
    if total > digits {
        Some(format!("Ensure that there are no more than {digits} digits in total."))
    } else if scale > places {
        Some(format!("Ensure that there are no more than {places} decimal places."))
    } else if whole > digits - places {
        Some(format!("Ensure that there are no more than {} digits before the decimal point.", digits - places))
    } else {
        None
    }
}

pub(crate) fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            // Naive values are in the server zone; UTC matches TIME_ZONE handling of make_aware for a UTC server.
            return Some(n.and_utc());
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(d) = DateTime::parse_from_str(s, fmt) {
            return Some(d.with_timezone(&Utc));
        }
    }
    None
}

/// DRF field to_internal_value + model validators. Err = list of messages.
async fn coerce(pool: &PgPool, fl: &F, v: &Value) -> Result<B, Result<String, Response>> {
    let e = |m: String| Err(Ok(m));
    if v.is_null() {
        return if fl.nullable { Ok(B::Null) } else { e("This field may not be null.".into()) };
    }
    match fl.ty {
        Ty::Int(min, max) => match parse_int(v) {
            Some(i) if i < min => e(format!("Ensure this value is greater than or equal to {min}.")),
            Some(i) if i > max => e(format!("Ensure this value is less than or equal to {max}.")),
            Some(i) => Ok(B::I(i)),
            None => e("A valid integer is required.".into()),
        },
        Ty::Dec(digits, places, min, max) => {
            let Some(t) = as_text(v).filter(|_| !v.is_boolean()) else {
                return e("A valid number is required.".into());
            };
            let Ok(d) = Decimal::from_str(t.trim()).or_else(|_| Decimal::from_scientific(t.trim())) else {
                return e("A valid number is required.".into());
            };
            if let Some(m) = dec_check(&d, digits, places) {
                return e(m);
            }
            if let Some(mn) = min {
                if d < Decimal::from(mn) {
                    return e(format!("Ensure this value is greater than or equal to {mn}."));
                }
            }
            if let Some(mx) = max {
                if d > Decimal::from(mx) {
                    return e(format!("Ensure this value is less than or equal to {mx}."));
                }
            }
            Ok(B::D(d))
        }
        Ty::Str(maxlen, blank) => {
            let Some(s) = as_text(v) else { return e("Not a valid string.".into()) };
            let s = s.trim().to_string();
            if s.is_empty() && !blank {
                return e("This field may not be blank.".into());
            }
            if s.chars().count() > maxlen {
                return e(format!("Ensure this field has no more than {maxlen} characters."));
            }
            Ok(B::S(s))
        }
        Ty::Bool => {
            let t = match v {
                Value::Bool(b) => Some(*b),
                Value::Number(n) => match n.as_f64() {
                    Some(x) if x == 1.0 => Some(true),
                    Some(x) if x == 0.0 => Some(false),
                    _ => None,
                },
                Value::String(s) => match s.as_str() {
                    "true" | "True" | "TRUE" | "1" | "t" | "T" | "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
                    "false" | "False" | "FALSE" | "0" | "f" | "F" | "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
                    _ => None,
                },
                _ => None,
            };
            match t {
                Some(b) => Ok(B::Bo(b)),
                None => e("Must be a valid boolean.".into()),
            }
        }
        Ty::Date => match v.as_str().and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()) {
            Some(d) => Ok(B::Da(d)),
            None => e("Date has wrong format. Use one of these formats instead: YYYY-MM-DD.".into()),
        },
        Ty::Ts => match v.as_str().and_then(parse_dt) {
            Some(d) => Ok(B::Ts(d)),
            None => e("Datetime has wrong format. Use one of these formats instead: YYYY-MM-DDThh:mm[:ss[.uuuuuu]][+HH:MM|-HH:MM|Z].".into()),
        },
        Ty::Json => Ok(B::J(v.clone())),
        Ty::Choice(choices) => {
            let s = as_text(v).unwrap_or_else(|| v.to_string());
            if choices.contains(&s.as_str()) {
                Ok(B::S(s))
            } else {
                e(format!("\"{s}\" is not a valid choice."))
            }
        }
        Ty::FkInt(exists, _) => {
            let Some(i) = (match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            }) else {
                return e(format!("Incorrect type. Expected pk value, received {}.", py_type(v)));
            };
            let found: Option<i32> = sqlx::query_scalar(exists).bind(i as i32).fetch_optional(pool).await.map_err(|e| Err(httpx::db_error(e)))?;
            if found.is_none() {
                return e(format!("Invalid pk \"{i}\" - object does not exist."));
            }
            Ok(B::I(i))
        }
        Ty::FkUuid(exists, _) => {
            let Some(u) = v.as_str().and_then(|s| Uuid::parse_str(s.trim()).ok()) else {
                if fl.name == "external_id" {
                    return e("Must be a valid UUID.".into());
                }
                return e(format!("Incorrect type. Expected pk value, received {}.", py_type(v)));
            };
            if fl.name == "external_id" {
                return Ok(B::U(u));
            }
            let found: Option<i32> = sqlx::query_scalar(exists).bind(u).fetch_optional(pool).await.map_err(|e| Err(httpx::db_error(e)))?;
            if found.is_none() {
                return e(format!("Invalid pk \"{u}\" - object does not exist."));
            }
            Ok(B::U(u))
        }
    }
}

/// `check_fk_ownership`: a missing object is a refusal; a malformed pk is
/// left to the serializer.
async fn owner_ok(pool: &PgPool, res: &Res, data: &Map<String, Value>, user: i32) -> Result<bool, Response> {
    for fl in res.fields {
        let (owner_sql, is_uuid) = match fl.ty {
            Ty::FkInt(_, Some(o)) => (o, false),
            Ty::FkUuid(_, Some(o)) => (o, true),
            _ => continue,
        };
        let Some(v) = data.get(fl.name) else { continue };
        if v.is_null() {
            continue;
        }
        let row: Option<i32> = if is_uuid {
            let Some(u) = v.as_str().and_then(|s| Uuid::parse_str(s).ok()) else { continue };
            sqlx::query_scalar(owner_sql).bind(u).fetch_optional(pool).await.map_err(httpx::db_error)?
        } else {
            let Some(i) = (match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            }) else { continue };
            sqlx::query_scalar(owner_sql).bind(i as i32).fetch_optional(pool).await.map_err(httpx::db_error)?
        };
        match row {
            None => return Ok(false),
            Some(owner) if owner != user => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

fn parse_pk(res: &Res, id: &str) -> Option<B> {
    if res.uuid_pk {
        Uuid::parse_str(id).ok().map(B::U)
    } else {
        id.parse::<i32>().ok().map(|i| B::I(i as i64))
    }
}

fn owner_where(q: &mut QueryBuilder<'_, sqlx::Postgres>, res: &Res, user: i32) {
    let mut parts = res.owner.split("$U");
    q.push(parts.next().unwrap_or(""));
    for p in parts {
        q.push_bind(user);
        q.push(p);
    }
}

async fn exists_owned(pool: &PgPool, res: &Res, pk: &B, user: i32) -> Result<bool, Response> {
    let mut q = QueryBuilder::new(format!("SELECT 1 FROM {} WHERE id = ", res.table));
    push_b(&mut q, pk.clone());
    q.push(" AND ");
    owner_where(&mut q, res, user);
    Ok(q.build().fetch_optional(pool).await.map_err(httpx::db_error)?.is_some())
}

async fn validate_fields(
    pool: &PgPool,
    res: &Res,
    data: &Map<String, Value>,
    creating: bool,
    partial: bool,
) -> Result<Vec<(&'static F, B)>, Response> {
    let mut errors = Map::new();
    let mut out = Vec::new();
    for fl in res.fields {
        match data.get(fl.name) {
            Some(v) => match coerce(pool, fl, v).await {
                Ok(b) => out.push((fl, b)),
                Err(Ok(msg)) => {
                    errors.insert(fl.name.into(), json!([msg]));
                }
                Err(Err(resp)) => return Err(resp),
            },
            None => {
                if fl.required && (creating || !partial) {
                    errors.insert(fl.name.into(), json!(["This field is required."]));
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(bad(errors));
    }
    Ok(out)
}

fn get<'a>(vals: &'a [(&'static F, B)], name: &str) -> Option<&'a B> {
    vals.iter().find(|(f, _)| f.name == name).map(|(_, b)| b)
}

/// Serializer/model `validate()` hooks, run once fields are clean.
async fn object_validate(
    st: &AppState,
    res: &Res,
    user: &CurrentUser,
    vals: &mut Vec<(&'static F, B)>,
    pk: Option<&B>,
) -> Result<(), Response> {
    let pool = &st.pool;
    match res.kind {
        Kind::Routine => {
            let mut start = match get(vals, "start") { Some(B::Da(d)) => Some(*d), _ => None };
            let mut end = match get(vals, "end") { Some(B::Da(d)) => Some(*d), _ => None };
            if let Some(pk) = pk {
                let mut q = QueryBuilder::new("SELECT start, \"end\" FROM manager_routine WHERE id = ");
                push_b(&mut q, pk.clone());
                if let Some(row) = q.build_query_as::<(NaiveDate, NaiveDate)>().fetch_optional(pool).await.map_err(httpx::db_error)? {
                    start = start.or(Some(row.0));
                    end = end.or(Some(row.1));
                }
            }
            if let (Some(s), Some(e)) = (start, end) {
                if s > e {
                    return Err(bad(json!({"end": ["The end date cannot be before the start date."]}).as_object().unwrap().clone()));
                }
                let max_days: i64 = std::env::var("WGER_ROUTINE_MAX_DURATION_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
                if (e - s).num_days() > max_days {
                    return Err(bad(json!({"end": [format!("A routine cannot span more than {max_days} days.")]}).as_object().unwrap().clone()));
                }
            }
            // Routine.save(): is_public only for templates (needs effective is_template)
            let tmpl = match get(vals, "is_template") {
                Some(B::Bo(b)) => Some(*b),
                _ => None,
            };
            let tmpl = match (tmpl, pk) {
                (Some(t), _) => t,
                (None, Some(pk)) => {
                    let mut q = QueryBuilder::new("SELECT is_template FROM manager_routine WHERE id = ");
                    push_b(&mut q, pk.clone());
                    q.build_query_scalar::<bool>().fetch_one(pool).await.map_err(httpx::db_error)?
                }
                (None, None) => false,
            };
            if !tmpl {
                if let Some(slot) = vals.iter_mut().find(|(f, _)| f.name == "is_public") {
                    slot.1 = B::Bo(false);
                } else if pk.is_some() {
                    vals.push((ROUTINE_FIELDS.iter().find(|f| f.name == "is_public").unwrap(), B::Bo(false)));
                }
            }
        }
        Kind::Config => {
            if let Some(B::J(req)) = get(vals, "requirements") {
                if let Some(msg) = requirements_error(req) {
                    return Err(bad(json!({"requirements": [msg]}).as_object().unwrap().clone()));
                }
            }
            // Integer configs (sets/rest) store an integer column; Django
            // casts on save. Values with decimals are rounded the same way
            // Python int() would truncate them through the DecimalField.
            if matches!(res.table, "manager_setsconfig" | "manager_maxsetsconfig" | "manager_restconfig" | "manager_maxrestconfig") {
                if let Some(slot) = vals.iter_mut().find(|(f, _)| f.name == "value") {
                    if let B::D(d) = &slot.1 {
                        slot.1 = B::D(d.trunc());
                    }
                }
            }
        }
        Kind::SlotEntry => {
            if pk.is_none() {
                let slot_id = match get(vals, "slot") { Some(B::I(i)) => *i as i32, _ => 0 };
                let (rr, wr): (Option<Decimal>, Option<Decimal>) = sqlx::query_as(
                    "SELECT p.repetitions_rounding, p.weight_rounding FROM manager_slot s JOIN manager_day d ON d.id = s.day_id \
                     JOIN manager_routine r ON r.id = d.routine_id JOIN core_userprofile p ON p.user_id = r.user_id WHERE s.id = $1",
                )
                .bind(slot_id)
                .fetch_optional(pool)
                .await
                .map_err(httpx::db_error)?
                .unwrap_or((None, None));
                for (name, val) in [("repetition_rounding", rr), ("weight_rounding", wr)] {
                    let empty = match get(vals, name) { None | Some(B::Null) => true, Some(B::D(d)) => d.is_zero(), _ => false };
                    if empty {
                        let fl = ENTRY_FIELDS.iter().find(|f| f.name == name).unwrap();
                        vals.retain(|(f, _)| f.name != name);
                        vals.push((fl, val.map(B::D).unwrap_or(B::Null)));
                    }
                }
                if matches!(get(vals, "order"), None | Some(B::Null)) {
                    let max: Option<i32> = sqlx::query_scalar("SELECT MAX(\"order\") FROM manager_slotentry WHERE slot_id = $1")
                        .bind(slot_id)
                        .fetch_one(pool)
                        .await
                        .map_err(httpx::db_error)?;
                    let fl = ENTRY_FIELDS.iter().find(|f| f.name == "order").unwrap();
                    vals.retain(|(f, _)| f.name != "order");
                    vals.push((fl, B::I(max.unwrap_or(0) as i64 + 1)));
                }
            }
        }
        Kind::Log => {
            // WorkoutLog.clean() on the merged instance
            let mut merged: Map<String, Value> = Map::new();
            if let Some(pk) = pk {
                let mut q = QueryBuilder::new("SELECT repetitions IS NOT NULL, weight IS NOT NULL, repetitions_unit_id IS NOT NULL, weight_unit_id IS NOT NULL FROM manager_workoutlog WHERE id = ");
                push_b(&mut q, pk.clone());
                let (r, w, ru, wu) = q.build_query_as::<(bool, bool, bool, bool)>().fetch_one(pool).await.map_err(httpx::db_error)?;
                for (k, b) in [("repetitions", r), ("weight", w), ("repetitions_unit", ru), ("weight_unit", wu)] {
                    merged.insert(k.into(), if b { json!(1) } else { Value::Null });
                }
            }
            for (fl, b) in vals.iter() {
                merged.insert(fl.name.into(), if matches!(b, B::Null) { Value::Null } else { json!(1) });
            }
            let p = |k: &str| merged.get(k).map(|v| !v.is_null()).unwrap_or(false);
            let msg = if !p("repetitions") && !p("weight") {
                Some("Both repetitions and weight cannot be null at the same time.")
            } else if p("repetitions") && !p("repetitions_unit") {
                Some("Repetitions unit must be present if repetitions have a value.")
            } else if p("weight") && !p("weight_unit") {
                Some("Weight unit must be present if weight has a value.")
            } else {
                None
            };
            if let Some(m) = msg {
                return Err(bad(json!({"non_field_errors": [m]}).as_object().unwrap().clone()));
            }
        }
        Kind::Measurement => {
            if let Some(B::U(cat)) = get(vals, "category") {
                let row: Option<(String, String, bool)> = sqlx::query_as(
                    "SELECT metric_type, dynamic_type, EXISTS(SELECT 1 FROM measurements_category c WHERE c.parent_id = $1) FROM measurements_category WHERE id = $1",
                )
                .bind(*cat)
                .fetch_optional(pool)
                .await
                .map_err(httpx::db_error)?;
                if let Some((metric, dynamic, children)) = row {
                    let msg = if children {
                        Some("Measurements cannot be added to a category with subcategories".to_string())
                    } else if matches!(metric.as_str(), "blood_pressure") {
                        Some(format!("Measurements cannot be added to a {metric} category, they belong in its components"))
                    } else if dynamic != "none" {
                        Some("The entries of a calculated category are maintained by the server".to_string())
                    } else {
                        None
                    };
                    if let Some(m) = msg {
                        return Err(bad(json!({"category": [m]}).as_object().unwrap().clone()));
                    }
                }
            }
            if let Some(B::J(extra)) = get(vals, "extra_data") {
                if !extra.is_object() {
                    return Err(bad(json!({"extra_data": ["extra_data has to be a JSON object"]}).as_object().unwrap().clone()));
                }
                for key in ["min", "max"] {
                    if let Some(v) = extra.get(key) {
                        if !v.is_null() && !v.is_number() {
                            let ty = py_type(v);
                            return Err(bad(json!({"extra_data": [format!("The {key} of an aggregate has to be a number, not {ty}")]}).as_object().unwrap().clone()));
                        }
                    }
                }
            }
        }
        Kind::Weight => {
            let unit: String = sqlx::query_scalar("SELECT weight_unit FROM core_userprofile WHERE user_id = $1")
                .bind(user.id)
                .fetch_optional(pool)
                .await
                .map_err(httpx::db_error)?
                .unwrap_or_else(|| "kg".into());
            if let Some(B::D(w)) = get(vals, "weight") {
                let (min, max) = if unit == "lb" { (Decimal::from(22), Decimal::from(770)) } else { (Decimal::from(10), Decimal::from(350)) };
                if *w < min || *w > max {
                    return Err(bad(json!({"weight": [format!("Weight must be between {min} and {max} {unit}")]}).as_object().unwrap().clone()));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn requirements_error(v: &Value) -> Option<String> {
    if v.is_null() {
        return None;
    }
    let Some(obj) = v.as_object() else { return Some("Requirements must be a JSON object.".into()) };
    let Some(rules) = obj.get("rules") else { return Some("Missing required keys: 'rules'".into()) };
    let Some(rules) = rules.as_array() else { return Some("'rules' must be a list.".into()) };
    for r in rules {
        let s = r.as_str().unwrap_or("");
        if !["weight", "repetitions", "rir", "rest"].contains(&s) {
            return Some(format!("Invalid rule: {}", py_repr(r)));
        }
    }
    None
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

async fn auth(st: &AppState, h: &HeaderMap) -> Result<CurrentUser, Response> {
    httpx::require_user(st, h).await
}

async fn create(st: &AppState, h: &HeaderMap, res: &'static Res, body: Bytes) -> Response {
    let user = match auth(st, h).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let data = match parse_body(&body) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match owner_ok(&st.pool, res, &data, user.id).await {
        Ok(true) => {}
        Ok(false) => return forbidden(),
        Err(r) => return r,
    }
    let mut vals = match validate_fields(&st.pool, res, &data, true, false).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = object_validate(st, res, &user, &mut vals, None).await {
        return r;
    }
    let pk = if res.uuid_pk { B::U(Uuid::now_v7()) } else { B::Null };
    let mut extra: Vec<(&str, B)> = Vec::new();
    match res.kind {
        Kind::Log => {
            let mut m = Map::new();
            for (fl, b) in vals.iter() {
                let v = match b {
                    B::U(u) => json!(u.to_string()),
                    B::I(i) => json!(i),
                    B::Ts(t) => json!(t.to_rfc3339()),
                    B::Null => Value::Null,
                    _ => continue,
                };
                m.insert(fl.name.into(), v);
            }
            if !m.contains_key("date") {
                m.insert("date".into(), json!(Utc::now().to_rfc3339()));
                vals.push((LOG_FIELDS.iter().find(|f| f.name == "date").unwrap(), B::Ts(Utc::now())));
            }
            if let Err(r) = super::upload::prepare_workoutlog(&st.pool, user.id, &mut m).await {
                return r;
            }
            if let Some(Value::String(s)) = m.get("session") {
                if let Ok(u) = Uuid::parse_str(s) {
                    vals.retain(|(f, _)| f.name != "session");
                    vals.push((LOG_FIELDS.iter().find(|f| f.name == "session").unwrap(), B::U(u)));
                }
            }
        }
        Kind::Weight => {
            let (cat, unit) = match body_weight_category(&st.pool, user.id).await {
                Ok(v) => v,
                Err(r) => return r,
            };
            extra.push(("category_id", B::U(cat)));
            extra.push(("extra_data", B::J(json!({"unit": unit}))));
            extra.push(("notes", B::S(String::new())));
            extra.push(("source", B::S("manual".into())));
        }
        _ => {}
    }
    let mut q = QueryBuilder::new(format!("INSERT INTO {} (", res.table));
    let mut cols: Vec<String> = Vec::new();
    if res.uuid_pk {
        cols.push("id".into());
    }
    for (fl, _) in vals.iter() {
        cols.push(format!("\"{}\"", fl.column));
    }
    let defaults: Vec<&F> = res.fields.iter().filter(|fl| !vals.iter().any(|(v, _)| v.name == fl.name) && fl.default.is_some()).collect();
    for fl in &defaults {
        cols.push(format!("\"{}\"", fl.column));
    }
    if res.kind == Kind::Routine {
        cols.push("created".into());
    }
    if res.force_user {
        cols.push("user_id".into());
    }
    for (c, _) in &extra {
        cols.push(format!("\"{c}\""));
    }
    q.push(cols.join(", "));
    q.push(") VALUES (");
    let mut first = true;
    let mut sep = |q: &mut QueryBuilder<'_, sqlx::Postgres>| {
        if !first {
            q.push(", ");
        }
        first = false;
    };
    if res.uuid_pk {
        sep(&mut q);
        push_b(&mut q, pk.clone());
    }
    for (_, b) in vals.iter() {
        sep(&mut q);
        push_b(&mut q, b.clone());
    }
    for fl in &defaults {
        sep(&mut q);
        q.push(fl.default.unwrap());
    }
    if res.kind == Kind::Routine {
        sep(&mut q);
        q.push("now()");
    }
    if res.force_user {
        sep(&mut q);
        q.push_bind(user.id);
    }
    for (_, b) in extra {
        sep(&mut q);
        push_b(&mut q, b);
    }
    q.push(") RETURNING id::text");
    let id: String = match q.build_query_scalar().fetch_one(&st.pool).await {
        Ok(id) => id,
        Err(e) => return integrity(e),
    };
    if res.kind == Kind::Log {
        let mut m = Map::new();
        if let Some(B::U(s)) = get(&vals, "session") {
            m.insert("session".into(), json!(s.to_string()));
        }
        if let Some(B::I(e)) = get(&vals, "slot_entry") {
            m.insert("slot_entry".into(), json!(e));
        }
        let _ = super::upload::backfill_session_day(&st.pool, &m).await;
    }
    with_status((res.render)(st, h, id).await, StatusCode::CREATED)
}

async fn body_weight_category(pool: &PgPool, user: i32) -> Result<(Uuid, String), Response> {
    let unit: String = sqlx::query_scalar("SELECT weight_unit FROM core_userprofile WHERE user_id = $1")
        .bind(user)
        .fetch_optional(pool)
        .await
        .map_err(httpx::db_error)?
        .unwrap_or_else(|| "kg".into());
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM measurements_category WHERE user_id = $1 AND is_official AND metric_type = 'body_weight' LIMIT 1",
    )
    .bind(user)
    .fetch_optional(pool)
    .await
    .map_err(httpx::db_error)?;
    if let Some(id) = existing {
        return Ok((id, unit));
    }
    Err(httpx::json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"detail": "body weight category missing (Category.get_or_create_body_weight not ported)"}),
    ))
}

fn integrity(e: sqlx::Error) -> Response {
    if let sqlx::Error::Database(db) = &e {
        if db.code().map(|c| c.starts_with("23") || c.starts_with("22")).unwrap_or(false) {
            return httpx::json_response(StatusCode::BAD_REQUEST, json!({"non_field_errors": [db.message()]}));
        }
    }
    httpx::db_error(e)
}

fn with_status(mut r: Response, s: StatusCode) -> Response {
    if r.status() == StatusCode::OK {
        *r.status_mut() = s;
    }
    r
}

async fn update(st: &AppState, h: &HeaderMap, res: &'static Res, id: String, body: Bytes, partial: bool) -> Response {
    let user = match auth(st, h).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let data = match parse_body(&body) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match owner_ok(&st.pool, res, &data, user.id).await {
        Ok(true) => {}
        Ok(false) => return forbidden(),
        Err(r) => return r,
    }
    let Some(pk) = parse_pk(res, &id) else { return httpx::not_found() };
    match exists_owned(&st.pool, res, &pk, user.id).await {
        Ok(true) => {}
        Ok(false) => return httpx::not_found(),
        Err(r) => return r,
    }
    let mut vals = match validate_fields(&st.pool, res, &data, false, partial).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(r) = object_validate(st, res, &user, &mut vals, Some(&pk)).await {
        return r;
    }
    let mut sets: Vec<(String, B)> = vals.into_iter().map(|(f, b)| (f.column.to_string(), b)).collect();
    if res.kind == Kind::Weight && sets.iter().any(|(c, _)| c == "value") {
        let unit: String = sqlx::query_scalar("SELECT weight_unit FROM core_userprofile WHERE user_id = $1")
            .bind(user.id)
            .fetch_optional(&st.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "kg".into());
        sets.push(("__unit".into(), B::S(unit)));
    }
    if !sets.is_empty() {
        let mut q = QueryBuilder::new(format!("UPDATE {} SET ", res.table));
        for (i, (c, b)) in sets.into_iter().enumerate() {
            if i > 0 {
                q.push(", ");
            }
            if c == "__unit" {
                q.push("extra_data = extra_data || jsonb_build_object('unit', ");
                push_b(&mut q, b);
                q.push("::text)");
                continue;
            }
            q.push(format!("\"{c}\" = "));
            push_b(&mut q, b);
        }
        q.push(" WHERE id = ");
        push_b(&mut q, pk.clone());
        if let Err(e) = q.build().execute(&st.pool).await {
            return integrity(e);
        }
    }
    if res.table == "manager_day" {
        // Day.save(): rest days lose their slots
        let day = id.parse::<i32>().unwrap_or(0);
        let slots: Vec<i32> = sqlx::query_scalar("SELECT s.id FROM manager_slot s JOIN manager_day d ON d.id = s.day_id WHERE d.id = $1 AND d.is_rest")
            .bind(day)
            .fetch_all(&st.pool)
            .await
            .unwrap_or_default();
        for slot in slots {
            if let Err(e) = delete_cascade(&st.pool, "manager_slot", &B::I(slot as i64)).await {
                return integrity(e);
            }
        }
    }
    (res.render)(st, h, id).await
}

async fn destroy(st: &AppState, h: &HeaderMap, res: &'static Res, id: String) -> Response {
    let user = match auth(st, h).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(pk) = parse_pk(res, &id) else { return httpx::not_found() };
    match exists_owned(&st.pool, res, &pk, user.id).await {
        Ok(true) => {}
        Ok(false) => return httpx::not_found(),
        Err(r) => return r,
    }
    match delete_cascade(&st.pool, res.table, &pk).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => integrity(e),
    }
}

const CONFIG_TABLES: &[&str] = &[
    "manager_weightconfig", "manager_maxweightconfig", "manager_repetitionsconfig", "manager_maxrepetitionsconfig",
    "manager_setsconfig", "manager_maxsetsconfig", "manager_restconfig", "manager_maxrestconfig",
    "manager_rirconfig", "manager_maxrirconfig",
];

/// Django `on_delete=CASCADE` for the routine tree (DB constraints are
/// NO ACTION, deferred), done in one transaction.
async fn delete_cascade(pool: &PgPool, table: &str, pk: &B) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (int_id, uuid_id) = match pk {
        B::I(i) => (*i as i32, Uuid::nil()),
        B::U(u) => (0, *u),
        _ => (0, Uuid::nil()),
    };
    let ids = |v: &[i32]| v.to_vec();
    let mut routines: Vec<i32> = vec![];
    let mut days: Vec<i32> = vec![];
    let mut slots: Vec<i32> = vec![];
    let mut entries: Vec<i32> = vec![];
    let mut sessions: Vec<Uuid> = vec![];
    let mut logs: Vec<Uuid> = vec![];
    match table {
        "manager_routine" => routines.push(int_id),
        "manager_day" => days.push(int_id),
        "manager_slot" => slots.push(int_id),
        "manager_slotentry" => entries.push(int_id),
        "manager_workoutlog" => logs.push(uuid_id),
        t if CONFIG_TABLES.contains(&t) => {
            sqlx::query(&format!("DELETE FROM {t} WHERE id = $1")).bind(int_id).execute(&mut *tx).await?;
            return tx.commit().await;
        }
        t => {
            let mut q = QueryBuilder::new(format!("DELETE FROM {t} WHERE id = "));
            push_b(&mut q, pk.clone());
            q.build().execute(&mut *tx).await?;
            return tx.commit().await;
        }
    }
    let more: Vec<i32> = sqlx::query_scalar("SELECT id FROM manager_day WHERE routine_id = ANY($1)").bind(ids(&routines)).fetch_all(&mut *tx).await?;
    days.extend(more);
    let more: Vec<i32> = sqlx::query_scalar("SELECT id FROM manager_slot WHERE day_id = ANY($1)").bind(ids(&days)).fetch_all(&mut *tx).await?;
    slots.extend(more);
    let more: Vec<i32> = sqlx::query_scalar("SELECT id FROM manager_slotentry WHERE slot_id = ANY($1)").bind(ids(&slots)).fetch_all(&mut *tx).await?;
    entries.extend(more);
    let more: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM manager_workoutsession WHERE routine_id = ANY($1) OR day_id = ANY($2)")
        .bind(ids(&routines)).bind(ids(&days)).fetch_all(&mut *tx).await?;
    sessions.extend(more);
    let more: Vec<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE l(id) AS ( \
           SELECT id FROM manager_workoutlog WHERE id = ANY($4) OR routine_id = ANY($1) OR session_id = ANY($2) OR slot_entry_id = ANY($3) \
           UNION SELECT w.id FROM manager_workoutlog w JOIN l ON w.next_log_id = l.id) SELECT id FROM l",
    )
    .bind(ids(&routines)).bind(sessions.clone()).bind(ids(&entries)).bind(logs.clone())
    .fetch_all(&mut *tx).await?;
    logs = more;
    sqlx::query("DELETE FROM manager_workoutlog WHERE id = ANY($1)").bind(&logs).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM manager_workoutsession WHERE id = ANY($1)").bind(&sessions).execute(&mut *tx).await?;
    for t in CONFIG_TABLES {
        sqlx::query(&format!("DELETE FROM {t} WHERE slot_entry_id = ANY($1)")).bind(ids(&entries)).execute(&mut *tx).await?;
    }
    sqlx::query("DELETE FROM manager_slotentry WHERE id = ANY($1)").bind(ids(&entries)).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM manager_slot WHERE id = ANY($1)").bind(ids(&slots)).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM manager_day WHERE id = ANY($1)").bind(ids(&days)).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM manager_label WHERE routine_id = ANY($1)").bind(ids(&routines)).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM manager_routine WHERE id = ANY($1)").bind(ids(&routines)).execute(&mut *tx).await?;
    tx.commit().await
}

#[allow(dead_code)]
fn _unused(_: Method) {}
