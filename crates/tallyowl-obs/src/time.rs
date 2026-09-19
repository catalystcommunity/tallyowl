//! Time, from `docs/CONVENTIONS.md` section 7.
//!
//! TallyOwl stores and transmits milliseconds since the Unix epoch. It keeps
//! event time, receive time, and commit time as three separate facts and never
//! collapses them. A duration field ends in `_ms`. Storage is Coordinated
//! Universal Time; a display converts to the reader's timezone.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch, now. Corndogs takes its sweep time in this
/// unit, so the conversion lives here rather than at each call site.
pub fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Coordinated Universal Time in the `YYYY-MM-DDTHH:MM:SS.mmmZ` form, for a log
/// line that a person reads. Storage and transport keep the millisecond value.
pub fn to_utc_text(ms: i64) -> String {
    let (days, ms_of_day) = {
        let d = ms.div_euclid(86_400_000);
        let r = ms.rem_euclid(86_400_000);
        (d, r)
    };
    let (year, month, day) = civil_from_days(days);
    let seconds_of_day = ms_of_day / 1000;
    let millis = ms_of_day % 1000;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// Howard Hinnant's `civil_from_days`. It is exact for every day this project
/// can represent, and it removes a dependency that only formats a date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_formats_as_the_epoch() {
        assert_eq!(to_utc_text(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_known_instant_formats_exactly() {
        // 2026-08-02T00:00:00Z is 1785628800 seconds after the epoch.
        assert_eq!(to_utc_text(1_785_628_800_000), "2026-08-02T00:00:00.000Z");
        assert_eq!(to_utc_text(1_785_628_800_123), "2026-08-02T00:00:00.123Z");
    }

    #[test]
    fn a_leap_day_formats_exactly() {
        // 2024-02-29T12:34:56Z
        assert_eq!(to_utc_text(1_709_210_096_000), "2024-02-29T12:34:56.000Z");
    }

    #[test]
    fn an_instant_before_the_epoch_stays_correct() {
        assert_eq!(to_utc_text(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn now_is_after_the_design_work() {
        assert!(now_ms() > 1_780_000_000_000);
        assert!(now_nanos() / 1_000_000 >= now_ms() - 1_000);
    }
}
