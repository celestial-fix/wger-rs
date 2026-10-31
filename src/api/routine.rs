//! Load a routine into the date-sequence engine.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::engine::{ChangeConfig, Day, LogSnap, Routine, Slot, SlotEntry};
use crate::jsonutil::dec_str;
use crate::timefmt::{fmt_dt, local_date};

pub async fn load_routine(
    pool: &PgPool,
    id: i32,
    owner_tz: Tz,
) -> Result<Option<Routine>, sqlx::Error> {
    let routine = sqlx::query(
        r#"
        SELECT r.id, r.name, r.description, r.created, r.start, r.end, r.fit_in_week,
               COALESCE(NULLIF(p.time_zone, ''), '') AS time_zone
        FROM manager_routine r
        JOIN auth_user u ON u.id = r.user_id
        LEFT JOIN core_userprofile p ON p.user_id = u.id
        WHERE r.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(routine) = routine else {
        return Ok(None);
    };
    let zone_name: String = routine.get("time_zone");
    let tz = zone_name.parse::<Tz>().unwrap_or(owner_tz);
    let start: NaiveDate = routine.get("start");
    let end: NaiveDate = routine.get("end");
    let created: DateTime<Utc> = routine.get("created");

    let day_rows = sqlx::query(
        r#"
        SELECT id, routine_id, "order", name, description, is_rest, need_logs_to_advance, type, config
        FROM manager_day WHERE routine_id = $1 ORDER BY "order", id
        "#,
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let slot_rows = sqlx::query(
        r#"
        SELECT s.id, s.day_id, s."order", s.comment, s.config
        FROM manager_slot s
        JOIN manager_day d ON d.id = s.day_id
        WHERE d.routine_id = $1
        ORDER BY s."order", s.id
        "#,
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let entry_rows = sqlx::query(
        r#"
        SELECT e.id, e.slot_id, e.exercise_id, e."order", e.comment, e.type, e.class_name, e.config,
               e.repetition_unit_id, ru.name AS repetition_unit_name, e.repetition_rounding,
               e.weight_unit_id, wu.name AS weight_unit_name, e.weight_rounding
        FROM manager_slotentry e
        JOIN manager_slot s ON s.id = e.slot_id
        JOIN manager_day d ON d.id = s.day_id
        LEFT JOIN core_repetitionunit ru ON ru.id = e.repetition_unit_id
        LEFT JOIN core_weightunit wu ON wu.id = e.weight_unit_id
        WHERE d.routine_id = $1
        ORDER BY e."order", e.id
        "#,
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let sessions = sqlx::query(
        "SELECT day_id, datetime_start FROM manager_workoutsession WHERE routine_id = $1 AND day_id IS NOT NULL",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut session_dates: HashMap<i32, HashSet<NaiveDate>> = HashMap::new();
    for row in &sessions {
        let day_id: i32 = row.get("day_id");
        let ts: DateTime<Utc> = row.get("datetime_start");
        session_dates
            .entry(day_id)
            .or_default()
            .insert(local_date(ts, tz));
    }

    let logs = sqlx::query(
        r#"
        SELECT slot_entry_id, iteration, weight, repetitions, rir, rest
        FROM manager_workoutlog
        WHERE routine_id = $1 AND slot_entry_id IS NOT NULL
        ORDER BY date, id
        "#,
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut logs_by_entry: HashMap<i32, Vec<LogSnap>> = HashMap::new();
    for row in &logs {
        let entry_id: i32 = row.get("slot_entry_id");
        logs_by_entry.entry(entry_id).or_default().push(LogSnap {
            iteration: row.get("iteration"),
            weight: row.get("weight"),
            repetitions: row.get("repetitions"),
            rir: row.get("rir"),
            rest: row.get::<Option<i32>, _>("rest").map(Decimal::from),
        });
    }

    let mut configs: HashMap<i32, EntryConfigs> = HashMap::new();
    load_dec_configs(
        pool,
        id,
        "manager_repetitionsconfig",
        2,
        &mut configs,
        |c, v| c.reps = v,
    )
    .await?;
    load_dec_configs(
        pool,
        id,
        "manager_maxrepetitionsconfig",
        2,
        &mut configs,
        |c, v| c.max_reps = v,
    )
    .await?;
    load_dec_configs(pool, id, "manager_weightconfig", 2, &mut configs, |c, v| {
        c.weight = v
    })
    .await?;
    load_dec_configs(
        pool,
        id,
        "manager_maxweightconfig",
        2,
        &mut configs,
        |c, v| c.max_weight = v,
    )
    .await?;
    load_dec_configs(pool, id, "manager_rirconfig", 1, &mut configs, |c, v| {
        c.rir = v
    })
    .await?;
    load_dec_configs(pool, id, "manager_maxrirconfig", 1, &mut configs, |c, v| {
        c.max_rir = v
    })
    .await?;
    load_int_configs(pool, id, "manager_setsconfig", &mut configs, |c, v| {
        c.sets = v
    })
    .await?;
    load_int_configs(pool, id, "manager_maxsetsconfig", &mut configs, |c, v| {
        c.max_sets = v
    })
    .await?;
    load_int_configs(pool, id, "manager_restconfig", &mut configs, |c, v| {
        c.rest = v
    })
    .await?;
    load_int_configs(pool, id, "manager_maxrestconfig", &mut configs, |c, v| {
        c.max_rest = v
    })
    .await?;

    let label_rows = sqlx::query(
        "SELECT start_offset, end_offset, label FROM manager_label WHERE routine_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let mut labels = HashMap::new();
    for row in &label_rows {
        let start_off: i32 = row.get("start_offset");
        let end_off: i32 = row.get("end_offset");
        let label: String = row.get("label");
        let mut off = start_off;
        while off <= end_off {
            if let Some(date) = start.checked_add_signed(chrono::Duration::days(off as i64)) {
                labels.insert(date, label.clone());
            }
            off += 1;
        }
    }

    let mut entries_by_slot: HashMap<i32, Vec<SlotEntry>> = HashMap::new();
    for row in &entry_rows {
        let slot_id: i32 = row.get("slot_id");
        let entry_id: i32 = row.get("id");
        let cfgs = configs.remove(&entry_id).unwrap_or_default();
        let class_name: Option<String> = row.get("class_name");
        entries_by_slot.entry(slot_id).or_default().push(SlotEntry {
            id: entry_id as i64,
            slot: slot_id as i64,
            exercise: row.get::<i32, _>("exercise_id") as i64,
            order: row.get::<i32, _>("order") as i64,
            comment: row.get("comment"),
            type_name: row.get("type"),
            class_name,
            config: json_opt(row),
            repetition_unit: row
                .get::<Option<i32>, _>("repetition_unit_id")
                .map(|v| v as i64),
            repetition_unit_name: row.get("repetition_unit_name"),
            repetition_rounding: row.get("repetition_rounding"),
            weight_unit: row
                .get::<Option<i32>, _>("weight_unit_id")
                .map(|v| v as i64),
            weight_unit_name: row.get("weight_unit_name"),
            weight_rounding: row.get("weight_rounding"),
            repetitions_configs: cfgs.reps,
            max_repetitions_configs: cfgs.max_reps,
            weight_configs: cfgs.weight,
            max_weight_configs: cfgs.max_weight,
            set_nr_configs: cfgs.sets,
            max_set_nr_configs: cfgs.max_sets,
            rir_configs: cfgs.rir,
            max_rir_configs: cfgs.max_rir,
            rest_configs: cfgs.rest,
            max_rest_configs: cfgs.max_rest,
            logs: logs_by_entry.remove(&entry_id).unwrap_or_default(),
            value_kinds: Default::default(),
        });
    }

    let mut slots_by_day: HashMap<i32, Vec<Slot>> = HashMap::new();
    for row in &slot_rows {
        let day_id: i32 = row.get("day_id");
        let slot_id: i32 = row.get("id");
        slots_by_day.entry(day_id).or_default().push(Slot {
            id: slot_id as i64,
            day: day_id as i64,
            order: row.get::<i32, _>("order") as i64,
            comment: row.get("comment"),
            config: json_col(row, "config"),
            entries: entries_by_slot.remove(&slot_id).unwrap_or_default(),
        });
    }

    let days = day_rows
        .iter()
        .map(|row| {
            let day_id: i32 = row.get("id");
            Day {
                id: day_id as i64,
                routine: id as i64,
                order: row.get::<i32, _>("order") as i64,
                name: row.get("name"),
                description: row.get("description"),
                is_rest: row.get("is_rest"),
                need_logs_to_advance: row.get("need_logs_to_advance"),
                type_name: row.get("type"),
                config: json_col(row, "config"),
                slots: slots_by_day.remove(&day_id).unwrap_or_default(),
                session_dates: session_dates.remove(&day_id).unwrap_or_default(),
            }
        })
        .collect();

    Ok(Some(Routine {
        id: id as i64,
        name: routine.get("name"),
        description: routine.get("description"),
        created: fmt_dt(created, tz),
        start,
        end,
        fit_in_week: routine.get("fit_in_week"),
        days,
        labels,
    }))
}

#[derive(Default)]
struct EntryConfigs {
    reps: Vec<ChangeConfig>,
    max_reps: Vec<ChangeConfig>,
    weight: Vec<ChangeConfig>,
    max_weight: Vec<ChangeConfig>,
    rir: Vec<ChangeConfig>,
    max_rir: Vec<ChangeConfig>,
    sets: Vec<ChangeConfig>,
    max_sets: Vec<ChangeConfig>,
    rest: Vec<ChangeConfig>,
    max_rest: Vec<ChangeConfig>,
}

async fn load_dec_configs<F>(
    pool: &PgPool,
    routine_id: i32,
    table: &str,
    scale: u32,
    into: &mut HashMap<i32, EntryConfigs>,
    slot: F,
) -> Result<(), sqlx::Error>
where
    F: Fn(&mut EntryConfigs, Vec<ChangeConfig>),
{
    let sql = format!(
        r#"
        SELECT c.id, c.slot_entry_id, c.iteration, c.value, c.operation, c.step, c.repeat, c.requirements
        FROM {table} c
        JOIN manager_slotentry e ON e.id = c.slot_entry_id
        JOIN manager_slot s ON s.id = e.slot_id
        JOIN manager_day d ON d.id = s.day_id
        WHERE d.routine_id = $1
        ORDER BY c.slot_entry_id, c.iteration, c.id
        "#
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(routine_id)
        .fetch_all(pool)
        .await?;
    group_configs(rows, into, slot, |row| {
        let value: Decimal = row.get("value");
        (value, Value::String(dec_str(value, scale)))
    });
    Ok(())
}

async fn load_int_configs<F>(
    pool: &PgPool,
    routine_id: i32,
    table: &str,
    into: &mut HashMap<i32, EntryConfigs>,
    slot: F,
) -> Result<(), sqlx::Error>
where
    F: Fn(&mut EntryConfigs, Vec<ChangeConfig>),
{
    let sql = format!(
        r#"
        SELECT c.id, c.slot_entry_id, c.iteration, c.value, c.operation, c.step, c.repeat, c.requirements
        FROM {table} c
        JOIN manager_slotentry e ON e.id = c.slot_entry_id
        JOIN manager_slot s ON s.id = e.slot_id
        JOIN manager_day d ON d.id = s.day_id
        WHERE d.routine_id = $1
        ORDER BY c.slot_entry_id, c.iteration, c.id
        "#
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
        .bind(routine_id)
        .fetch_all(pool)
        .await?;
    group_configs(rows, into, slot, |row| {
        let value: i32 = row.get("value");
        (Decimal::from(value), json!(value))
    });
    Ok(())
}

fn group_configs<F, G>(
    rows: Vec<PgRow>,
    into: &mut HashMap<i32, EntryConfigs>,
    slot: F,
    value_of: G,
) where
    F: Fn(&mut EntryConfigs, Vec<ChangeConfig>),
    G: Fn(&PgRow) -> (Decimal, Value),
{
    let mut current: Option<i32> = None;
    let mut buf = Vec::new();
    let flush =
        |into: &mut HashMap<i32, EntryConfigs>, id: i32, buf: Vec<ChangeConfig>, slot: &F| {
            let entry = into.entry(id).or_default();
            slot(entry, buf);
        };
    for row in &rows {
        let entry_id: i32 = row.get("slot_entry_id");
        if let Some(previous) = current.filter(|id| *id != entry_id) {
            flush(into, previous, std::mem::take(&mut buf), &slot);
        }
        current = Some(entry_id);
        let (value, value_json) = value_of(row);
        let requirements: Option<sqlx::types::Json<Value>> = row.get("requirements");
        buf.push(ChangeConfig {
            id: row.get::<i32, _>("id") as i64,
            slot_entry: entry_id as i64,
            iteration: row.get("iteration"),
            value,
            value_json,
            operation: row.get("operation"),
            step: row.get("step"),
            repeat: row.get("repeat"),
            requirements: requirements.map(|j| j.0).unwrap_or(Value::Null),
        });
    }
    if let Some(id) = current {
        flush(into, id, buf, &slot);
    }
}

fn json_opt(row: &PgRow) -> Option<Value> {
    let value: Option<sqlx::types::Json<Value>> = row.get("config");
    value.map(|j| j.0)
}

fn json_col(row: &PgRow, _name: &str) -> Option<Value> {
    json_opt(row)
}
