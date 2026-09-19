//! Calendar periods, in the timezone a query supplied.
//!
//! **Everything is stored in UTC and stays in UTC.** This module converts at one
//! place and for one reason: a question about a *calendar* period cannot be
//! answered by arithmetic on a UTC instant. "The day that starts at midnight in
//! Berlin" is 23 hours long in March and 25 in October, and a calendar month is
//! 28, 29, 30, or 31 days. A dashboard that answered "this month" with a fixed
//! span would disagree with the reader's own records, and the reader would be
//! right.
//!
//! `docs/QUERY.md` section 12.2 asks for calendar periods "in the supplied
//! timezone", and `TimeRange.timezone` has carried the name since the contract
//! was written. This is what reads it.
//!
//! # What this is not for
//!
//! A fixed interval — five minutes, an hour, a day counted as 86,400,000
//! milliseconds — never comes here. It is arithmetic on an instant and a
//! timezone cannot change it. Only a `calendar` interval and a retention period
//! reach this module.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use tallyowl_obs::error::TallyOwlError;

/// A calendar unit, in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Hour,
    Day,
    Week,
    Month,
}

impl Unit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Unit::Hour => "hour",
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
        }
    }
}

/// The zone a calendar question is asked in. UTC unless the query named one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone(Tz);

impl Default for Zone {
    fn default() -> Zone {
        Zone(Tz::UTC)
    }
}

impl Zone {
    /// Read a zone name, or refuse it by name.
    ///
    /// CONVENTIONS.md section 5: a refusal names what was wrong and gives a
    /// valid example. An unknown zone is a caller mistake and answering it in
    /// UTC would be answering a different question without saying so.
    pub fn named(name: Option<&str>) -> Result<Zone, TallyOwlError> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(Zone(Tz::UTC));
        };
        match name.parse::<Tz>() {
            Ok(zone) => Ok(Zone(zone)),
            Err(_) => Err(TallyOwlError::invalid_argument(format!(
                "`{name}` is not a timezone this installation knows. Use an IANA zone name such \
                 as `Europe/Berlin`, `America/New_York`, or `UTC`."
            ))),
        }
    }

    pub fn name(&self) -> &str {
        self.0.name()
    }

    pub fn is_utc(&self) -> bool {
        self.0 == Tz::UTC
    }

    fn local(&self, at_ms: i64) -> DateTime<Tz> {
        let instant = DateTime::<Utc>::from_timestamp_millis(at_ms)
            // A millisecond outside the representable range is not a real
            // observation. Clamping keeps a bucket function total, and the
            // caller's range check is what refuses the query.
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_nanos(0));
        instant.with_timezone(&self.0)
    }

    /// The instant a local wall-clock time names, as UTC milliseconds.
    ///
    /// **A wall-clock time can be missing or repeated**, which is what a
    /// daylight-saving boundary means. A spring-forward skips an hour, so
    /// 02:30 does not exist; an autumn boundary repeats one, so 02:30 happens
    /// twice. The earlier of a repeated pair is taken, and a missing time steps
    /// forward until it exists, because a bucket boundary has to be one instant
    /// and the alternative is a query that fails twice a year.
    fn instant(&self, local: chrono::NaiveDateTime) -> i64 {
        use chrono::offset::LocalResult;
        match self.0.from_local_datetime(&local) {
            LocalResult::Single(at) => at.timestamp_millis(),
            LocalResult::Ambiguous(earlier, _later) => earlier.timestamp_millis(),
            LocalResult::None => {
                // The gap is never more than a few hours in any zone the
                // database holds. Stepping by the minute finds the first
                // wall-clock time that exists.
                let mut walked = local;
                for _ in 0..(6 * 60) {
                    walked += Duration::minutes(1);
                    if let LocalResult::Single(at) = self.0.from_local_datetime(&walked) {
                        return at.timestamp_millis();
                    }
                }
                // Unreachable against a real database, and a total function is
                // worth more here than a panic.
                local.and_utc().timestamp_millis()
            }
        }
    }

    /// The start of the calendar period `at_ms` falls in, as UTC milliseconds.
    pub fn start_of(&self, at_ms: i64, unit: Unit) -> i64 {
        let local = self.local(at_ms).naive_local();
        let floored = match unit {
            Unit::Hour => local.date().and_hms_opt(local.hour(), 0, 0),
            Unit::Day => local.date().and_hms_opt(0, 0, 0),
            Unit::Week => {
                // A week starts on Monday. ISO-8601 is what
                // `docs/DOCUMENTATION.md` already binds every other date rule
                // to, and a week that started on Sunday in one report and
                // Monday in another would be two answers to one question.
                let back = local.weekday().num_days_from_monday() as i64;
                (local.date() - Duration::days(back)).and_hms_opt(0, 0, 0)
            }
            Unit::Month => NaiveDate::from_ymd_opt(local.year(), local.month(), 1)
                .and_then(|date| date.and_hms_opt(0, 0, 0)),
        };
        match floored {
            Some(floored) => self.instant(floored),
            None => at_ms,
        }
    }

    /// `count` calendar periods after the start of the one `at_ms` falls in.
    ///
    /// **This is what a fixed span cannot do.** Adding a month to 31 January
    /// gives 28 February in an ordinary year, and adding one to that gives 31
    /// March rather than 28 March, because each step is taken from the period
    /// start and not from the day.
    pub fn advance(&self, at_ms: i64, unit: Unit, count: i64) -> i64 {
        let start = self.start_of(at_ms, unit);
        if count == 0 {
            return start;
        }
        let local = self.local(start).naive_local();
        let moved = match unit {
            Unit::Hour => Some(local + Duration::hours(count)),
            Unit::Day => Some(local + Duration::days(count)),
            Unit::Week => Some(local + Duration::weeks(count)),
            Unit::Month => {
                let months = local.year() as i64 * 12 + (local.month() as i64 - 1) + count;
                let year = months.div_euclid(12);
                let month = months.rem_euclid(12) + 1;
                i32::try_from(year).ok().and_then(|year| {
                    NaiveDate::from_ymd_opt(year, month as u32, 1)
                        .and_then(|date| date.and_hms_opt(0, 0, 0))
                })
            }
        };
        match moved {
            Some(moved) => self.instant(moved),
            None => start,
        }
    }

    /// How many whole calendar periods separate two instants.
    ///
    /// Negative when `at_ms` is before `from_ms`, which a caller reads as "this
    /// row is before the cohort it would join".
    pub fn periods_between(&self, from_ms: i64, at_ms: i64, unit: Unit) -> i64 {
        let from = self.local(self.start_of(from_ms, unit)).naive_local();
        let to = self.local(self.start_of(at_ms, unit)).naive_local();
        match unit {
            Unit::Month => {
                (to.year() as i64 * 12 + to.month() as i64)
                    - (from.year() as i64 * 12 + from.month() as i64)
            }
            // A day and a week are whole numbers of local days apart once both
            // ends are floored, so the difference of the dates is exact and
            // does not inherit the 23-hour day.
            Unit::Day => (to.date() - from.date()).num_days(),
            Unit::Week => (to.date() - from.date()).num_days() / 7,
            Unit::Hour => (to - from).num_hours(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-29 is the spring boundary in Berlin: 02:00 becomes 03:00, so the
    /// local day is 23 hours long.
    const BERLIN_SPRING: &str = "Europe/Berlin";

    fn at(text: &str) -> i64 {
        DateTime::parse_from_rfc3339(text)
            .expect("a timestamp")
            .timestamp_millis()
    }

    #[test]
    fn an_absent_timezone_is_utc_and_nothing_moves() {
        let zone = Zone::named(None).expect("no zone is UTC");
        assert!(zone.is_utc());
        assert_eq!(zone.name(), "UTC");
        assert_eq!(
            zone.start_of(at("2026-08-08T13:47:11Z"), Unit::Day),
            at("2026-08-08T00:00:00Z")
        );
    }

    #[test]
    fn a_timezone_this_installation_does_not_know_is_refused_by_name() {
        // Answering it in UTC would answer a different question and say
        // nothing about having done so.
        let failure = Zone::named(Some("Middle/Earth")).expect_err("an unknown zone is refused");
        assert!(failure.to_string().contains("Middle/Earth"));
        assert!(failure.to_string().contains("Europe/Berlin"));
    }

    #[test]
    fn a_day_starts_at_local_midnight_and_not_at_utc_midnight() {
        let zone = Zone::named(Some(BERLIN_SPRING)).expect("a zone");
        // 00:30 in Berlin on 10 August is 22:30 UTC on 9 August. The day it
        // belongs to starts at 22:00 UTC on 9 August, not at 00:00 UTC on 10.
        assert_eq!(
            zone.start_of(at("2026-08-09T22:30:00Z"), Unit::Day),
            at("2026-08-09T22:00:00Z")
        );
    }

    #[test]
    fn a_day_across_the_spring_boundary_is_twenty_three_hours() {
        // The whole reason this module exists. A fixed 86,400,000 would put
        // every bucket after this one an hour out for the rest of the summer.
        let zone = Zone::named(Some(BERLIN_SPRING)).expect("a zone");
        let start = zone.start_of(at("2026-03-29T05:00:00Z"), Unit::Day);
        let next = zone.advance(start, Unit::Day, 1);
        assert_eq!(
            next - start,
            23 * 3_600_000,
            "the local day was counted as a fixed span"
        );
    }

    #[test]
    fn a_day_across_the_autumn_boundary_is_twenty_five_hours() {
        let zone = Zone::named(Some(BERLIN_SPRING)).expect("a zone");
        let start = zone.start_of(at("2026-10-25T05:00:00Z"), Unit::Day);
        let next = zone.advance(start, Unit::Day, 1);
        assert_eq!(next - start, 25 * 3_600_000);
    }

    #[test]
    fn a_month_is_the_month_it_is_and_never_twenty_eight_days() {
        // L110 chose 28 days because 28 is obviously not a month and 30 nearly
        // is. Neither is a month.
        let zone = Zone::named(None).expect("UTC");
        let lengths: Vec<i64> = ["2026-01-15", "2026-02-15", "2026-04-15", "2024-02-15"]
            .iter()
            .map(|day| {
                let start = zone.start_of(at(&format!("{day}T00:00:00Z")), Unit::Month);
                (zone.advance(start, Unit::Month, 1) - start) / 86_400_000
            })
            .collect();
        assert_eq!(
            lengths,
            vec![31, 28, 30, 29],
            "February 2024 is a leap year"
        );
    }

    #[test]
    fn a_month_step_is_taken_from_the_month_and_not_from_the_day() {
        // Adding thirty-one days twice from 31 January lands in March and then
        // in April. Adding a month twice lands in February and then in March,
        // which is what a person means.
        let zone = Zone::named(None).expect("UTC");
        let january = zone.start_of(at("2026-01-31T09:00:00Z"), Unit::Month);
        assert_eq!(
            zone.advance(january, Unit::Month, 1),
            at("2026-02-01T00:00:00Z")
        );
        assert_eq!(
            zone.advance(january, Unit::Month, 2),
            at("2026-03-01T00:00:00Z")
        );
        assert_eq!(
            zone.advance(january, Unit::Month, 14),
            at("2027-03-01T00:00:00Z")
        );
    }

    #[test]
    fn a_week_starts_on_monday() {
        let zone = Zone::named(None).expect("UTC");
        // 2026-08-08 is a Saturday.
        assert_eq!(
            zone.start_of(at("2026-08-08T13:00:00Z"), Unit::Week),
            at("2026-08-03T00:00:00Z")
        );
    }

    #[test]
    fn counting_periods_between_two_instants_survives_the_boundary() {
        let zone = Zone::named(Some(BERLIN_SPRING)).expect("a zone");
        let before = at("2026-03-28T12:00:00Z");
        let after = at("2026-03-30T12:00:00Z");
        assert_eq!(zone.periods_between(before, after, Unit::Day), 2);
        // And a fixed span would say one, because two 24-hour steps overshoot
        // a 23-hour day.
        assert_eq!((after - before) / 86_400_000, 2);
        assert_eq!(zone.periods_between(after, before, Unit::Day), -2);
    }

    #[test]
    fn counting_months_between_two_instants_is_a_calendar_count() {
        let zone = Zone::named(None).expect("UTC");
        assert_eq!(
            zone.periods_between(
                at("2026-01-31T00:00:00Z"),
                at("2026-03-01T00:00:00Z"),
                Unit::Month
            ),
            2
        );
    }

    #[test]
    fn a_bucket_boundary_that_the_local_clock_skips_still_has_one_instant() {
        // Spring forward in Santiago moves midnight itself: the local day
        // starts at 01:00, because 00:00 does not exist. A bucket boundary has
        // to be one instant, so it steps forward to the first that does.
        let zone = Zone::named(Some("America/Santiago")).expect("a zone");
        let start = zone.start_of(at("2026-09-06T12:00:00Z"), Unit::Day);
        let next = zone.advance(start, Unit::Day, 1);
        assert!(next > start, "a day that skips midnight had no length");
        assert_eq!(next - start, 24 * 3_600_000);
    }
}
