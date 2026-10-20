//! Routine date-sequence engine.
//!
//! Port of `Routine.date_sequence`, `SlotEntry.get_config_data`, slot display/gym
//! shaping, and `SetConfigData.text_repr` from wger 2.7. English gettext strings
//! are used for the text representation (`Sets`, `Reps`, `RiR`, `rest`).

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate};
use rust_decimal::prelude::*;
use rust_decimal::RoundingStrategy;
use serde_json::{json, Map, Value};

const REP_UNIT_REPETITIONS: i64 = 1;
const REP_UNIT_TILL_FAILURE: i64 = 2;
const MAX_COMPOUND_VALUE: &str = "9999.99";
const MAX_COMPOUND_RIR: &str = "9.5";

const PROGRESSION_FIELDS: [&str; 10] = [
    "weight",
    "maxweight",
    "repetitions",
    "maxrepetitions",
    "rir",
    "maxrir",
    "rest",
    "maxrest",
    "sets",
    "maxsets",
];

fn base_field(field: &str) -> Option<&'static str> {
    match field {
        "maxweight" => Some("weight"),
        "maxrepetitions" => Some("repetitions"),
        "maxrir" => Some("rir"),
        "maxrest" => Some("rest"),
        "maxsets" => Some("sets"),
        _ => None,
    }
}

fn field_cap(field: &str) -> Decimal {
    if field == "rir" || field == "maxrir" {
        Decimal::from_str(MAX_COMPOUND_RIR).unwrap()
    } else {
        Decimal::from_str(MAX_COMPOUND_VALUE).unwrap()
    }
}

#[derive(Clone, Debug)]
pub struct ChangeConfig {
    pub id: i64,
    pub slot_entry: i64,
    pub iteration: i32,
    pub value: Decimal,
    pub value_json: Value,
    pub operation: String,
    pub step: String,
    pub repeat: bool,
    pub requirements: Value,
}

impl ChangeConfig {
    fn replace(&self) -> bool {
        self.operation == "r"
    }

    fn requirements_rules(&self) -> Option<Vec<String>> {
        let req = &self.requirements;
        if req.is_null() {
            return None;
        }
        if let Some(obj) = req.as_object() {
            if obj.is_empty() {
                return None;
            }
            let rules = obj
                .get("rules")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if rules.is_empty() {
                None
            } else {
                Some(rules)
            }
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogSnap {
    pub iteration: Option<i32>,
    pub weight: Option<Decimal>,
    pub repetitions: Option<Decimal>,
    pub rir: Option<Decimal>,
    pub rest: Option<Decimal>,
}

impl LogSnap {
    fn field(&self, rule: &str) -> Option<Decimal> {
        match rule {
            "weight" => self.weight,
            "repetitions" => self.repetitions,
            "rir" => self.rir,
            "rest" => self.rest,
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SlotEntry {
    pub id: i64,
    pub slot: i64,
    pub exercise: i64,
    pub order: i64,
    pub comment: String,
    pub type_name: String,
    pub class_name: Option<String>,
    pub config: Option<Value>,
    pub repetition_unit: Option<i64>,
    pub repetition_unit_name: Option<String>,
    pub repetition_rounding: Option<Decimal>,
    pub weight_unit: Option<i64>,
    pub weight_unit_name: Option<String>,
    pub weight_rounding: Option<Decimal>,
    pub repetitions_configs: Vec<ChangeConfig>,
    pub max_repetitions_configs: Vec<ChangeConfig>,
    pub weight_configs: Vec<ChangeConfig>,
    pub max_weight_configs: Vec<ChangeConfig>,
    pub set_nr_configs: Vec<ChangeConfig>,
    pub max_set_nr_configs: Vec<ChangeConfig>,
    pub rir_configs: Vec<ChangeConfig>,
    pub max_rir_configs: Vec<ChangeConfig>,
    pub rest_configs: Vec<ChangeConfig>,
    pub max_rest_configs: Vec<ChangeConfig>,
    pub logs: Vec<LogSnap>,
    /// How each config list serializes its `value` (integer vs decimal scale).
    pub value_kinds: ValueKinds,
}

#[derive(Clone, Copy, Debug)]
pub struct ValueKinds {
    pub repetitions: ValueKind,
    pub weight: ValueKind,
    pub sets: ValueKind,
    pub rir: ValueKind,
    pub rest: ValueKind,
}

impl Default for ValueKinds {
    fn default() -> Self {
        Self {
            repetitions: ValueKind::Decimal(2),
            weight: ValueKind::Decimal(2),
            sets: ValueKind::Integer,
            rir: ValueKind::Decimal(1),
            rest: ValueKind::Integer,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ValueKind {
    Integer,
    Decimal(u32),
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub id: i64,
    pub day: i64,
    pub order: i64,
    pub comment: String,
    pub config: Option<Value>,
    pub entries: Vec<SlotEntry>,
}

#[derive(Clone, Debug)]
pub struct Day {
    pub id: i64,
    pub routine: i64,
    pub order: i64,
    pub name: String,
    pub description: String,
    pub is_rest: bool,
    pub need_logs_to_advance: bool,
    pub type_name: String,
    pub config: Option<Value>,
    pub slots: Vec<Slot>,
    /// Local dates on which this day has a session (owner timezone).
    pub session_dates: HashSet<NaiveDate>,
}

#[derive(Clone, Debug)]
pub struct Routine {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub created: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub fit_in_week: bool,
    pub days: Vec<Day>,
    pub labels: HashMap<NaiveDate, String>,
}

#[derive(Clone, Debug)]
struct WalkState {
    value: Option<Decimal>,
    active: Option<ChangeConfig>,
    armed: Option<ChangeConfig>,
}

impl WalkState {
    fn new() -> Self {
        Self {
            value: None,
            active: None,
            armed: None,
        }
    }

    fn advance(&mut self, config: Option<&ChangeConfig>) -> Option<ChangeConfig> {
        if let Some(config) = config {
            self.active = Some(config.clone());
            self.armed = None;
            return Some(config.clone());
        }
        if self.active.as_ref().is_some_and(|c| c.repeat) {
            return self.active.clone();
        }
        self.armed.clone()
    }

    fn apply(&mut self, candidate: &ChangeConfig, is_open: bool, max_value: Decimal) {
        if is_open {
            self.value = Some(apply_config_value(self.value, candidate, max_value));
            self.armed = None;
        } else if !candidate.repeat {
            self.armed = Some(candidate.clone());
        }
    }
}

fn apply_config_value(
    value: Option<Decimal>,
    config: &ChangeConfig,
    max_value: Decimal,
) -> Decimal {
    let mut out = value.unwrap_or(Decimal::ZERO);
    if config.replace() {
        out = config.value;
    } else {
        let step = if config.step == "abs" {
            config.value
        } else {
            out * config.value / Decimal::from(100)
        };
        if config.operation == "+" {
            out += step;
        } else {
            out -= step;
        }
    }
    if out > max_value {
        out = max_value;
    }
    out
}

fn round_half_even_int(q: Decimal) -> Decimal {
    q.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven)
}

pub fn round_value(x: Option<Decimal>, base: Option<Decimal>) -> Option<Decimal> {
    let x = x?;
    let result = match base {
        None => x,
        Some(b) if b.is_zero() => x,
        Some(b) => b * round_half_even_int(x / b),
    };
    if result.fract().is_zero() {
        Some(result.trunc())
    } else {
        Some(result)
    }
}

fn truthy_dec(v: Option<Decimal>) -> bool {
    v.is_some_and(|d| !d.is_zero())
}

fn display_rounding(entry: &SlotEntry, field: &str) -> Option<Decimal> {
    match field {
        "weight" => entry.weight_rounding,
        "repetitions" => entry.repetition_rounding,
        "rir" => Some(Decimal::new(5, 1)),
        "rest" => Some(Decimal::ONE),
        _ => None,
    }
}

fn requirements_met(
    rules: &[String],
    logs: &[LogSnap],
    thresholds: &HashMap<&str, Option<Decimal>>,
) -> bool {
    logs.iter().any(|log| {
        rules.iter().all(|rule| {
            let log_value = log.field(rule);
            let threshold = thresholds.get(rule.as_str()).copied().flatten();
            match (log_value, threshold) {
                (Some(lv), Some(th)) => lv >= th,
                _ => false,
            }
        })
    })
}

fn configs_for<'a>(entry: &'a SlotEntry, field: &str) -> &'a [ChangeConfig] {
    match field {
        "weight" => &entry.weight_configs,
        "maxweight" => &entry.max_weight_configs,
        "repetitions" => &entry.repetitions_configs,
        "maxrepetitions" => &entry.max_repetitions_configs,
        "rir" => &entry.rir_configs,
        "maxrir" => &entry.max_rir_configs,
        "rest" => &entry.rest_configs,
        "maxrest" => &entry.max_rest_configs,
        "sets" => &entry.set_nr_configs,
        "maxsets" => &entry.max_set_nr_configs,
        _ => &[],
    }
}

#[derive(Clone, Debug)]
struct SetData {
    slot_entry_id: i64,
    exercise: i64,
    sets: i64,
    max_sets: Option<i64>,
    weight: Option<Decimal>,
    max_weight: Option<Decimal>,
    weight_unit: Option<i64>,
    weight_rounding: Option<Decimal>,
    repetitions: Option<Decimal>,
    max_repetitions: Option<Decimal>,
    repetitions_unit: Option<i64>,
    repetitions_rounding: Option<Decimal>,
    rir: Option<Decimal>,
    max_rir: Option<Decimal>,
    rest: Option<Decimal>,
    max_rest: Option<Decimal>,
    type_name: String,
    comment: String,
    weight_unit_name: Option<String>,
    repetitions_unit_name: Option<String>,
}

fn dec_to_i64(v: Option<Decimal>) -> Option<i64> {
    v.and_then(|d| d.trunc().to_i64())
}

fn get_config_data(entry: &SlotEntry, iteration: i32) -> SetData {
    let target = iteration.max(1);
    let mut by_field: HashMap<&str, HashMap<i32, ChangeConfig>> = HashMap::new();
    for field in PROGRESSION_FIELDS {
        let map = configs_for(entry, field)
            .iter()
            .filter(|c| c.iteration <= target)
            .map(|c| (c.iteration, c.clone()))
            .collect();
        by_field.insert(field, map);
    }
    let mut states: HashMap<&str, WalkState> = PROGRESSION_FIELDS
        .iter()
        .map(|f| (*f, WalkState::new()))
        .collect();

    let mut logs_by_iter: HashMap<i32, Vec<&LogSnap>> = HashMap::new();
    for log in &entry.logs {
        if let Some(iter) = log.iteration {
            logs_by_iter.entry(iter).or_default().push(log);
        }
    }

    for i in 1..=target {
        let thresholds: HashMap<&str, Option<Decimal>> = ["weight", "repetitions", "rir", "rest"]
            .into_iter()
            .map(|rule| {
                let value = states.get(rule).and_then(|s| s.value);
                (rule, round_value(value, display_rounding(entry, rule)))
            })
            .collect();
        let log_data = logs_by_iter.get(&(i - 1)).cloned().unwrap_or_default();
        let log_owned: Vec<LogSnap> = log_data.into_iter().cloned().collect();

        let mut candidates: HashMap<&str, Option<ChangeConfig>> = HashMap::new();
        for field in PROGRESSION_FIELDS {
            let config = by_field.get(field).and_then(|m| m.get(&i)).cloned();
            let cand = states.get_mut(field).unwrap().advance(config.as_ref());
            candidates.insert(field, cand);
        }

        for field in PROGRESSION_FIELDS {
            let candidate = match candidates.get(field).and_then(|c| c.clone()) {
                Some(c) => c,
                None => continue,
            };
            let mut gate = candidate.clone();
            if let Some(base) = base_field(field) {
                if by_field.get(base).is_some_and(|m| !m.is_empty()) {
                    if let Some(base_cand) = candidates.get(base).and_then(|c| c.clone()) {
                        gate = base_cand;
                    } else if let Some(active) = states.get(base).and_then(|s| s.active.clone()) {
                        gate = active;
                    }
                }
            }
            let rules = gate.requirements_rules();
            let is_open = i == 1
                || rules.is_none()
                || requirements_met(rules.as_deref().unwrap_or(&[]), &log_owned, &thresholds);
            states
                .get_mut(field)
                .unwrap()
                .apply(&candidate, is_open, field_cap(field));
        }
    }

    let sets_raw = states["sets"].value;
    let max_sets_raw = states["maxsets"].value;
    let weight = states["weight"].value;
    let max_weight = states["maxweight"].value;
    let repetitions = states["repetitions"].value;
    let max_repetitions = states["maxrepetitions"].value;
    let rir = states["rir"].value;
    let max_rir = states["maxrir"].value;
    let rest = states["rest"].value;
    let max_rest = states["maxrest"].value;

    let sets = dec_to_i64(sets_raw).unwrap_or(1);
    let max_sets = dec_to_i64(round_value(max_sets_raw, Some(Decimal::ONE)));

    let weight_r = round_value(weight, display_rounding(entry, "weight"));
    let max_weight_r = match (max_weight, weight) {
        (Some(mw), Some(w)) if !mw.is_zero() && !w.is_zero() && mw > w => {
            round_value(Some(mw), display_rounding(entry, "weight"))
        }
        _ => None,
    };
    let reps_r = round_value(repetitions, display_rounding(entry, "repetitions"));
    let max_reps_r = match (max_repetitions, repetitions) {
        (Some(mr), Some(r)) if !mr.is_zero() && !r.is_zero() && mr > r => {
            round_value(Some(mr), display_rounding(entry, "repetitions"))
        }
        _ => None,
    };
    let rir_r = round_value(rir, display_rounding(entry, "rir"));
    let max_rir_r = match (max_rir, rir) {
        (Some(mr), Some(r)) if !mr.is_zero() && !r.is_zero() && mr > r => {
            round_value(Some(mr), display_rounding(entry, "rir"))
        }
        _ => None,
    };
    let rest_r = round_value(rest, display_rounding(entry, "rest"));
    let max_rest_r = match (max_rest, rest) {
        (Some(mr), Some(r)) if !mr.is_zero() && !r.is_zero() && mr > r => {
            round_value(Some(mr), display_rounding(entry, "rest"))
        }
        _ => None,
    };

    SetData {
        slot_entry_id: entry.id,
        exercise: entry.exercise,
        sets,
        max_sets,
        weight: weight_r,
        max_weight: max_weight_r,
        weight_rounding: if weight.is_some() {
            entry.weight_rounding
        } else {
            None
        },
        weight_unit: if weight.is_some() {
            entry.weight_unit
        } else {
            None
        },
        weight_unit_name: if weight.is_some() {
            entry.weight_unit_name.clone()
        } else {
            None
        },
        repetitions: reps_r,
        max_repetitions: max_reps_r,
        repetitions_rounding: if repetitions.is_some() {
            entry.repetition_rounding
        } else {
            None
        },
        repetitions_unit: if repetitions.is_some() {
            entry.repetition_unit
        } else {
            None
        },
        repetitions_unit_name: if repetitions.is_some() {
            entry.repetition_unit_name.clone()
        } else {
            None
        },
        rir: rir_r,
        max_rir: max_rir_r,
        rest: rest_r,
        max_rest: max_rest_r,
        type_name: entry.type_name.clone(),
        comment: entry.comment.clone(),
    }
}

fn fmt_display_number(d: Decimal) -> String {
    if d.fract().is_zero() {
        d.trunc().to_string()
    } else {
        let s = d.normalize().to_string();
        s
    }
}

fn text_repr(data: &SetData) -> String {
    let mut out: Vec<String> = Vec::new();
    if data.sets > 1 {
        let sets = if let Some(max) = data.max_sets.filter(|m| *m != 0) {
            format!("{}-{}", data.sets, max)
        } else {
            data.sets.to_string()
        };
        out.push(format!("{sets} Sets,"));
    }

    if truthy_dec(data.repetitions) {
        let reps_v = data.repetitions.unwrap();
        let mut reps = fmt_display_number(
            round_value(Some(reps_v), data.repetitions_rounding).unwrap_or(reps_v),
        );
        if let Some(max) = data.max_repetitions.filter(|m| !m.is_zero()) {
            let max_s = fmt_display_number(
                round_value(Some(max), data.repetitions_rounding).unwrap_or(max),
            );
            reps = format!("{reps}-{max_s}");
        }
        let mut unit = String::new();
        let ru = data.repetitions_unit;
        if ru.is_some_and(|u| u == REP_UNIT_REPETITIONS || u == REP_UNIT_TILL_FAILURE)
            && !truthy_dec(data.weight)
        {
            unit = "Reps".to_string();
        } else if ru == Some(REP_UNIT_TILL_FAILURE) {
            unit = "∞".to_string();
            reps.clear();
        } else if ru.is_some_and(|u| u != REP_UNIT_REPETITIONS && u != REP_UNIT_TILL_FAILURE) {
            unit = data.repetitions_unit_name.clone().unwrap_or_default();
        }
        let piece = format!("{reps} {unit}").trim().to_string();
        out.push(piece);
        if truthy_dec(data.weight) {
            out.push("×".to_string());
        }
    }

    if truthy_dec(data.weight) {
        let w = data.weight.unwrap();
        let mut weight =
            fmt_display_number(round_value(Some(w), data.weight_rounding).unwrap_or(w));
        if let Some(max) = data.max_weight.filter(|m| !m.is_zero()) {
            let max_s =
                fmt_display_number(round_value(Some(max), data.weight_rounding).unwrap_or(max));
            weight = format!("{weight}-{max_s}");
        }
        let unit = if data.weight_unit.is_some() {
            data.weight_unit_name.clone().unwrap_or_default()
        } else {
            String::new()
        };
        out.push(format!("{weight} {unit}").trim().to_string());
    }

    if truthy_dec(data.rir) {
        let rir = round_value(data.rir, Some(Decimal::new(5, 1))).unwrap_or(data.rir.unwrap());
        out.push(format!("@ {} RiR", fmt_display_number(rir)));
    }

    if truthy_dec(data.rest) {
        let mut rest = fmt_display_number(data.rest.unwrap());
        if let Some(max) = data.max_rest.filter(|m| !m.is_zero()) {
            rest = format!("{rest}-{}", fmt_display_number(max));
        }
        out.push(format!("{rest}s rest"));
    }

    let joined = out.join(" ");
    joined.trim_matches(',').to_string()
}

fn rpe_of(rir: Option<Decimal>) -> Option<Decimal> {
    let rir = rir?;
    if rir > Decimal::from(5) {
        Some(Decimal::from(4))
    } else {
        Some(Decimal::from(10) - rir)
    }
}

fn decimal_or_int(value: Option<Decimal>, places: u32) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    let q = value.round_dp_with_strategy(places, RoundingStrategy::MidpointNearestEven);
    if q.fract().is_zero() {
        json!(q.trunc().to_string())
    } else {
        json!(format_dec(q, places))
    }
}

fn format_dec(d: Decimal, places: u32) -> String {
    let q = d.round_dp_with_strategy(places, RoundingStrategy::MidpointNearestEven);
    let neg = q.is_sign_negative() && !q.is_zero();
    let abs = q.abs();
    let scale = Decimal::from(10u64.pow(places));
    let scaled = (abs * scale).trunc();
    let digits = scaled.to_string();
    let digits = if digits.len() < places as usize {
        format!("{:0>width$}", digits, width = places as usize)
    } else {
        digits
    };
    let split = digits.len() - places as usize;
    let (whole, frac) = digits.split_at(split);
    let whole = if whole.is_empty() { "0" } else { whole };
    let s = if places == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

fn dec_field(value: Option<Decimal>, places: u32) -> Value {
    match value {
        None => Value::Null,
        Some(v) => json!(format_dec(v, places)),
    }
}

fn set_json(data: &SetData) -> Value {
    json!({
        "slot_entry_id": data.slot_entry_id,
        "exercise": data.exercise,
        "sets": data.sets,
        "max_sets": data.max_sets,
        "weight": decimal_or_int(data.weight, 2),
        "max_weight": decimal_or_int(data.max_weight, 2),
        "weight_unit": data.weight_unit,
        "weight_rounding": dec_field(data.weight_rounding, 2),
        "repetitions": decimal_or_int(data.repetitions, 2),
        "max_repetitions": decimal_or_int(data.max_repetitions, 2),
        "repetitions_unit": data.repetitions_unit,
        "repetitions_rounding": dec_field(data.repetitions_rounding, 2),
        "rir": decimal_or_int(data.rir, 1),
        "max_rir": decimal_or_int(data.max_rir, 1),
        "rpe": decimal_or_int(rpe_of(data.rir), 1),
        "rest": decimal_or_int(data.rest, 2),
        "max_rest": decimal_or_int(data.max_rest, 2),
        "type": data.type_name,
        "text_repr": text_repr(data),
        "comment": data.comment,
    })
}

struct SlotOut {
    comment: String,
    sets: Vec<SetData>,
}

fn slot_exercises(sets: &[SetData]) -> Vec<i64> {
    let mut out = Vec::new();
    for set in sets {
        if !out.contains(&set.exercise) {
            out.push(set.exercise);
        }
    }
    out
}

fn slots_display(day: &Day, iteration: i32) -> Vec<SlotOut> {
    let mut out: Vec<SlotOut> = Vec::new();
    let mut last_exercise: Option<i64> = None;
    let mut current: Option<usize> = None;
    for slot in &day.slots {
        let sets: Vec<SetData> = slot
            .entries
            .iter()
            .map(|e| get_config_data(e, iteration))
            .collect();
        let exercises = slot_exercises(&sets);
        if exercises.len() > 1 {
            out.push(SlotOut {
                comment: slot.comment.clone(),
                sets,
            });
            current = None;
        } else if exercises.is_empty() {
            continue;
        } else {
            let exercise_id = exercises[0];
            if current.is_none() || Some(exercise_id) != last_exercise {
                out.push(SlotOut {
                    comment: slot.comment.clone(),
                    sets,
                });
                current = Some(out.len() - 1);
                last_exercise = Some(exercise_id);
            } else if let Some(idx) = current {
                out[idx].sets.extend(sets);
            }
        }
    }
    out
}

fn slots_gym(day: &Day, iteration: i32) -> Vec<SlotOut> {
    let mut out = Vec::new();
    for slot in &day.slots {
        let set_data: Vec<SetData> = slot
            .entries
            .iter()
            .map(|e| get_config_data(e, iteration))
            .collect();
        if set_data.len() == 1 {
            let mut data = set_data.into_iter().next().unwrap();
            let nr = data.sets.max(0);
            data.sets = 1;
            let sets = vec![data; nr as usize];
            out.push(SlotOut {
                comment: slot.comment.clone(),
                sets,
            });
            continue;
        }
        let mut counts: Vec<i64> = set_data.iter().map(|s| s.sets).collect();
        let mut result = Vec::new();
        while counts.iter().any(|c| *c > 0) {
            for (i, data) in set_data.iter().enumerate() {
                if counts[i] > 0 {
                    let mut one = data.clone();
                    one.sets = 1;
                    result.push(one);
                    counts[i] -= 1;
                }
            }
        }
        out.push(SlotOut {
            comment: slot.comment.clone(),
            sets: result,
        });
    }
    out
}

fn slot_out_json(slot: &SlotOut) -> Value {
    let exercises = slot_exercises(&slot.sets);
    json!({
        "comment": slot.comment,
        "is_superset": exercises.len() > 1,
        "exercises": exercises,
        "sets": slot.sets.iter().map(set_json).collect::<Vec<_>>(),
    })
}

fn day_json(day: &Day) -> Value {
    json!({
        "id": day.id,
        "routine": day.routine,
        "order": day.order,
        "name": day.name,
        "description": day.description,
        "is_rest": day.is_rest,
        "need_logs_to_advance": day.need_logs_to_advance,
        "type": day.type_name,
        "config": day.config.clone().unwrap_or(Value::Null),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceMode {
    Display,
    Gym,
}

pub fn date_sequence(routine: &Routine, today: NaiveDate, mode: SequenceMode) -> Vec<Value> {
    if routine.days.is_empty() {
        return Vec::new();
    }
    let days = &routine.days;
    let nr = days.len();
    let mut iteration_counter: HashMap<i64, i32> = HashMap::new();
    for day in days {
        iteration_counter.insert(day.id, 1);
    }
    let mut current_date = routine.start;
    let mut sequence = Vec::new();
    let mut is_first = true;
    let mut day_index: usize = 0;

    while current_date <= routine.end {
        let mut current_day = &days[day_index];
        let previous_date = current_date.pred_opt().unwrap_or(current_date);
        let has_session = current_day.session_dates.contains(&previous_date);
        let can_proceed = !current_day.need_logs_to_advance
            || (current_day.need_logs_to_advance && has_session)
            || current_date > today;
        let mut wrapped = false;
        if can_proceed && !is_first {
            *iteration_counter.entry(current_day.id).or_insert(1) += 1;
            day_index = (day_index + 1) % nr;
            wrapped = day_index == 0;
            current_day = &days[day_index];
        }
        if routine.fit_in_week && nr % 7 != 0 && wrapped {
            let days_to_monday = 7 - current_date.weekday().num_days_from_monday();
            for i in 0..days_to_monday {
                let placeholder_date = current_date + chrono::Duration::days(i as i64);
                if placeholder_date > routine.end {
                    break;
                }
                let iter = iteration_counter.get(&current_day.id).copied().unwrap_or(1) - 1;
                sequence.push(day_value(routine, None, placeholder_date, iter, mode));
            }
            current_date += chrono::Duration::days(days_to_monday as i64);
            if current_date > routine.end {
                continue;
            }
        }
        let iter = iteration_counter.get(&current_day.id).copied().unwrap_or(1);
        sequence.push(day_value(
            routine,
            Some(current_day),
            current_date,
            iter,
            mode,
        ));
        current_date += chrono::Duration::days(1);
        is_first = false;
    }
    sequence
}

fn day_value(
    routine: &Routine,
    day: Option<&Day>,
    date: NaiveDate,
    iteration: i32,
    mode: SequenceMode,
) -> Value {
    let label = routine
        .labels
        .get(&date)
        .cloned()
        .map(Value::String)
        .unwrap_or(Value::Null);
    let slots = match day {
        None => Vec::new(),
        Some(day) => match mode {
            SequenceMode::Display => slots_display(day, iteration)
                .iter()
                .map(slot_out_json)
                .collect(),
            SequenceMode::Gym => slots_gym(day, iteration)
                .iter()
                .map(slot_out_json)
                .collect(),
        },
    };
    let mut obj = Map::new();
    obj.insert("iteration".into(), json!(iteration));
    obj.insert("date".into(), json!(date.format("%Y-%m-%d").to_string()));
    obj.insert("label".into(), label);
    obj.insert("day".into(), day.map(day_json).unwrap_or(Value::Null));
    obj.insert("slots".into(), Value::Array(slots));
    Value::Object(obj)
}

fn parse_decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::Null => None,
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Decimal::from_str(s).ok()
            }
        }
        _ => None,
    }
}

fn parse_config(v: &Value) -> ChangeConfig {
    ChangeConfig {
        id: v.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
        slot_entry: v.get("slot_entry").and_then(|x| x.as_i64()).unwrap_or(0),
        iteration: v.get("iteration").and_then(|x| x.as_i64()).unwrap_or(1) as i32,
        value: parse_decimal(v.get("value").unwrap_or(&Value::Null)).unwrap_or(Decimal::ZERO),
        value_json: v.get("value").cloned().unwrap_or(Value::Null),
        operation: v
            .get("operation")
            .and_then(|x| x.as_str())
            .unwrap_or("r")
            .to_string(),
        step: v
            .get("step")
            .and_then(|x| x.as_str())
            .unwrap_or("na")
            .to_string(),
        repeat: v.get("repeat").and_then(|x| x.as_bool()).unwrap_or(false),
        requirements: v.get("requirements").cloned().unwrap_or(Value::Null),
    }
}

fn parse_configs(v: Option<&Value>) -> Vec<ChangeConfig> {
    v.and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_config).collect())
        .unwrap_or_default()
}

fn default_weight_name(id: Option<i64>) -> Option<String> {
    match id {
        Some(1) => Some("kg".into()),
        Some(2) => Some("lb".into()),
        Some(_) => Some("kg".into()),
        None => None,
    }
}

fn default_rep_name(id: Option<i64>) -> Option<String> {
    match id {
        Some(1) => Some("Repetitions".into()),
        Some(2) => Some("Until Failure".into()),
        Some(_) => Some("Repetitions".into()),
        None => None,
    }
}

fn configs_json(configs: &[ChangeConfig]) -> Value {
    Value::Array(
        configs
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "slot_entry": c.slot_entry,
                    "iteration": c.iteration,
                    "value": c.value_json,
                    "operation": c.operation,
                    "step": c.step,
                    "repeat": c.repeat,
                    "requirements": c.requirements,
                })
            })
            .collect(),
    )
}

fn entry_json(entry: &SlotEntry) -> Value {
    json!({
        "id": entry.id,
        "slot": entry.slot,
        "exercise": entry.exercise,
        "order": entry.order,
        "comment": entry.comment,
        "type": entry.type_name,
        "class_name": entry.class_name,
        "config": entry.config.clone().unwrap_or(Value::Null),
        "repetition_unit": entry.repetition_unit,
        "repetition_rounding": dec_field(entry.repetition_rounding, 2),
        "repetitions_configs": configs_json(&entry.repetitions_configs),
        "max_repetitions_configs": configs_json(&entry.max_repetitions_configs),
        "weight_unit": entry.weight_unit,
        "weight_rounding": dec_field(entry.weight_rounding, 2),
        "weight_configs": configs_json(&entry.weight_configs),
        "max_weight_configs": configs_json(&entry.max_weight_configs),
        "set_nr_configs": configs_json(&entry.set_nr_configs),
        "max_set_nr_configs": configs_json(&entry.max_set_nr_configs),
        "rir_configs": configs_json(&entry.rir_configs),
        "max_rir_configs": configs_json(&entry.max_rir_configs),
        "rest_configs": configs_json(&entry.rest_configs),
        "max_rest_configs": configs_json(&entry.max_rest_configs),
    })
}

pub fn structure_json(routine: &Routine) -> Value {
    let days: Vec<Value> = routine
        .days
        .iter()
        .map(|day| {
            let slots: Vec<Value> = day
                .slots
                .iter()
                .map(|slot| {
                    json!({
                        "id": slot.id,
                        "day": slot.day,
                        "order": slot.order,
                        "comment": slot.comment,
                        "entries": slot.entries.iter().map(entry_json).collect::<Vec<_>>(),
                        "config": slot.config.clone().unwrap_or(Value::Null),
                    })
                })
                .collect();
            let mut day_v = day_json(day);
            if let Some(obj) = day_v.as_object_mut() {
                obj.insert("slots".into(), Value::Array(slots));
            }
            day_v
        })
        .collect();
    json!({
        "id": routine.id,
        "name": routine.name,
        "description": routine.description,
        "created": routine.created,
        "start": routine.start.format("%Y-%m-%d").to_string(),
        "end": routine.end.format("%Y-%m-%d").to_string(),
        "fit_in_week": routine.fit_in_week,
        "days": days,
    })
}

pub fn routine_from_structure(body: &Value) -> Routine {
    let days = body
        .get("days")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut parsed_days = Vec::new();
    for day in days {
        let mut slots = Vec::new();
        for slot in day
            .get("slots")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let mut entries = Vec::new();
            for entry in slot
                .get("entries")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let wu = entry.get("weight_unit").and_then(|v| v.as_i64());
                let ru = entry.get("repetition_unit").and_then(|v| v.as_i64());
                entries.push(SlotEntry {
                    id: entry.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                    slot: entry.get("slot").and_then(|v| v.as_i64()).unwrap_or(0),
                    exercise: entry.get("exercise").and_then(|v| v.as_i64()).unwrap_or(0),
                    order: entry.get("order").and_then(|v| v.as_i64()).unwrap_or(0),
                    comment: entry
                        .get("comment")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    type_name: entry
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("normal")
                        .to_string(),
                    class_name: entry
                        .get("class_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    config: entry.get("config").filter(|v| !v.is_null()).cloned(),
                    repetition_unit: ru,
                    repetition_unit_name: entry
                        .get("repetition_unit_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| default_rep_name(ru)),
                    repetition_rounding: entry.get("repetition_rounding").and_then(parse_decimal),
                    weight_unit: wu,
                    weight_unit_name: entry
                        .get("weight_unit_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| default_weight_name(wu)),
                    weight_rounding: entry.get("weight_rounding").and_then(parse_decimal),
                    repetitions_configs: parse_configs(entry.get("repetitions_configs")),
                    max_repetitions_configs: parse_configs(entry.get("max_repetitions_configs")),
                    weight_configs: parse_configs(entry.get("weight_configs")),
                    max_weight_configs: parse_configs(entry.get("max_weight_configs")),
                    set_nr_configs: parse_configs(entry.get("set_nr_configs")),
                    max_set_nr_configs: parse_configs(entry.get("max_set_nr_configs")),
                    rir_configs: parse_configs(entry.get("rir_configs")),
                    max_rir_configs: parse_configs(entry.get("max_rir_configs")),
                    rest_configs: parse_configs(entry.get("rest_configs")),
                    max_rest_configs: parse_configs(entry.get("max_rest_configs")),
                    logs: Vec::new(),
                    value_kinds: ValueKinds::default(),
                });
            }
            slots.push(Slot {
                id: slot.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                day: slot.get("day").and_then(|v| v.as_i64()).unwrap_or(0),
                order: slot.get("order").and_then(|v| v.as_i64()).unwrap_or(0),
                comment: slot
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                config: slot.get("config").filter(|v| !v.is_null()).cloned(),
                entries,
            });
        }
        parsed_days.push(Day {
            id: day.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
            routine: day.get("routine").and_then(|v| v.as_i64()).unwrap_or(0),
            order: day.get("order").and_then(|v| v.as_i64()).unwrap_or(0),
            name: day
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            description: day
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            is_rest: day
                .get("is_rest")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            need_logs_to_advance: day
                .get("need_logs_to_advance")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            type_name: day
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("custom")
                .into(),
            config: day.get("config").filter(|v| !v.is_null()).cloned(),
            slots,
            session_dates: HashSet::new(),
        });
    }
    let start = NaiveDate::parse_from_str(
        body.get("start")
            .and_then(|v| v.as_str())
            .unwrap_or("1970-01-01"),
        "%Y-%m-%d",
    )
    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
    let end = NaiveDate::parse_from_str(
        body.get("end")
            .and_then(|v| v.as_str())
            .unwrap_or("1970-01-01"),
        "%Y-%m-%d",
    )
    .unwrap_or(start);
    Routine {
        id: body.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
        name: body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        description: body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        created: body
            .get("created")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        start,
        end,
        fit_in_week: body
            .get("fit_in_week")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        days: parsed_days,
        labels: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_routine() -> Value {
        json!({
            "id": 42,
            "name": "Sample routine",
            "description": "Synthetic test data",
            "created": "2026-01-01T00:00:00Z",
            "start": "2026-01-05",
            "end": "2026-01-10",
            "fit_in_week": false,
            "days": [
                {
                    "id": 101,
                    "routine": 42,
                    "order": 1,
                    "name": "Day A",
                    "description": "",
                    "is_rest": false,
                    "need_logs_to_advance": false,
                    "type": "custom",
                    "config": null,
                    "slots": []
                },
                {
                    "id": 102,
                    "routine": 42,
                    "order": 2,
                    "name": "Day B",
                    "description": "",
                    "is_rest": false,
                    "need_logs_to_advance": false,
                    "type": "custom",
                    "config": null,
                    "slots": []
                }
            ]
        })
    }

    #[test]
    fn synthetic_routine_structure_roundtrips() {
        let body = sample_routine();
        let routine = routine_from_structure(&body);
        assert_eq!(structure_json(&routine), body);
    }

    #[test]
    fn synthetic_routine_date_sequence_cycles_days() {
        let routine = routine_from_structure(&sample_routine());
        let today = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let got = date_sequence(&routine, today, SequenceMode::Display);
        let summary: Vec<_> = got
            .iter()
            .map(|day| {
                (
                    day["date"].as_str().unwrap().to_string(),
                    day["day"]["id"].as_i64().unwrap(),
                    day["iteration"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("2026-01-05".into(), 101, 1),
                ("2026-01-06".into(), 102, 1),
                ("2026-01-07".into(), 101, 2),
                ("2026-01-08".into(), 102, 2),
                ("2026-01-09".into(), 101, 3),
                ("2026-01-10".into(), 102, 3),
            ]
        );
    }

    #[test]
    fn round_value_strips_integral_scale() {
        let v = round_value(Decimal::from_str("45.00").ok(), None).unwrap();
        assert_eq!(v.to_string(), "45");
    }
}
