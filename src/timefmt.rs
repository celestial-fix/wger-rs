//! Django `datetime.isoformat()` in an active timezone.
//!
//! Microseconds are omitted when they are zero. The offset is `±HH:MM`, never `Z`.

use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;

pub fn fmt_dt(ts: DateTime<Utc>, tz: Tz) -> String {
    let local = ts.with_timezone(&tz);
    let us = local.timestamp_subsec_micros();
    // `%:z` is `±HH:MM`. Django's isoformat uses the same, never `Z` or `±HHMM`.
    let offset = local.format("%:z");
    let base = local.format("%Y-%m-%dT%H:%M:%S");
    if us == 0 {
        format!("{base}{offset}")
    } else {
        format!("{base}.{us:06}{offset}")
    }
}

pub fn fmt_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn fmt_time(t: NaiveTime) -> String {
    let us = t.nanosecond() / 1000;
    if us == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        format!("{}.{:06}", t.format("%H:%M:%S"), us)
    }
}

pub fn local_date(ts: DateTime<Utc>, tz: Tz) -> NaiveDate {
    ts.with_timezone(&tz).date_naive()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn isoformat_offset_and_micros() {
        let tz: Tz = "America/Santiago".parse().unwrap();
        let ts = Utc.with_ymd_and_hms(2023, 8, 6, 8, 17, 17).unwrap()
            + chrono::Duration::microseconds(422900);
        let formatted = fmt_dt(ts, tz);
        assert!(
            formatted.ends_with("-04:00") || formatted.ends_with("-03:00"),
            "{formatted}"
        );
        assert!(formatted.contains('.'), "{formatted}");
        assert!(!formatted.contains('Z'), "{formatted}");
        let whole = Utc.with_ymd_and_hms(2024, 1, 1, 15, 0, 0).unwrap();
        let plain = fmt_dt(whole, tz);
        assert!(!plain.contains('.'), "{plain}");
        assert!(
            plain.contains("-03:00") || plain.contains("-04:00"),
            "{plain}"
        );
    }
}
