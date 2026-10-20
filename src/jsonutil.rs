//! DRF field formatting: decimal strings, JSON numbers, nulls.

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{Number, Value};

pub fn dec_str(value: Decimal, scale: u32) -> String {
    let q = value.round_dp(scale);
    let negative = q.is_sign_negative() && !q.is_zero();
    let abs = q.abs();
    let raw = abs.to_string();
    let (whole, frac) = raw.split_once('.').unwrap_or((raw.as_str(), ""));
    let mut frac = frac.to_string();
    if frac.len() < scale as usize {
        frac.push_str(&"0".repeat(scale as usize - frac.len()));
    } else {
        frac.truncate(scale as usize);
    }
    let body = if scale == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

pub fn opt_dec_str(value: Option<Decimal>, scale: u32) -> Value {
    match value {
        Some(v) => Value::String(dec_str(v, scale)),
        None => Value::Null,
    }
}

/// Measurement values are JSON numbers (`coerce_to_string=False`), quantized first.
pub fn dec_number(value: Decimal, scale: u32) -> Value {
    let text = dec_str(value, scale);
    let num: f64 = text.parse().unwrap_or(0.0);
    Number::from_f64(num)
        .map(Value::Number)
        .unwrap_or(Value::from(0))
}

pub fn opt_i32(value: Option<i32>) -> Value {
    match value {
        Some(v) => Value::from(v),
        None => Value::Null,
    }
}

pub fn opt_i64(value: Option<i64>) -> Value {
    match value {
        Some(v) => Value::from(v),
        None => Value::Null,
    }
}

pub fn opt_str(value: Option<String>) -> Value {
    match value {
        Some(v) => Value::String(v),
        None => Value::Null,
    }
}

pub fn empty_as_null(value: Option<String>) -> Value {
    match value {
        Some(v) if !v.is_empty() => Value::String(v),
        _ => Value::Null,
    }
}

pub fn f64_from_dec(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

pub fn json_f64(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::from(0))
}
