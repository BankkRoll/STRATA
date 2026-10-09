//! UTC timestamps and the injectable clock.
//!
//! Every persisted time is UTC Unix seconds. Local time, DST and time zones
//! are a display concern of the UI; nothing in the store depends on them, so
//! retention thinning behaves identically on every machine and across DST
//! transitions.
//!
//! Store logic never reads the wall clock directly. It asks the [`Clock`] the
//! store was opened with, so tests (and the app's own diagnostics) can inject
//! synthetic dates through [`ManualClock`].

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use strata_core::FileTime;

const SECS_PER_DAY: i64 = 86_400;
const SECS_PER_HOUR: i64 = 3_600;

/// A UTC instant as whole seconds since 1970-01-01T00:00:00Z.
///
/// # Example
///
/// ```
/// use strata_store::Timestamp;
/// let t = Timestamp::from_utc(2026, 10, 9, 13, 30, 0).unwrap();
/// assert_eq!(t.to_string(), "2026-10-09T13:30:00Z");
/// assert_eq!(t.hour_start(), Timestamp::from_utc(2026, 10, 9, 13, 0, 0).unwrap());
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

/// A UTC calendar date and time, as produced by [`Timestamp::to_civil`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilUtc {
    /// Proleptic Gregorian year.
    pub year: i64,
    /// Month, 1-12.
    pub month: u32,
    /// Day of month, 1-31.
    pub day: u32,
    /// Hour, 0-23.
    pub hour: u32,
    /// Minute, 0-59.
    pub minute: u32,
    /// Second, 0-59.
    pub second: u32,
}

impl Timestamp {
    /// Builds a timestamp from a UTC calendar date and time.
    ///
    /// Returns `None` when any field is out of range (including February 30
    /// and the like).
    #[must_use]
    pub fn from_utc(
        year: i64,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        // Beyond this the day count overflows `i64` inside `days_from_civil`;
        // such instants are not representable as Unix seconds anyway.
        const MAX_YEAR: i64 = 200_000_000_000;
        if !(-MAX_YEAR..=MAX_YEAR).contains(&year)
            || !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }
        let days = days_from_civil(year, month, day);
        let secs = days
            .checked_mul(SECS_PER_DAY)?
            .checked_add(i64::from(hour * 3600 + minute * 60 + second))?;
        Some(Self(secs))
    }

    /// Converts a Windows FILETIME (second precision is kept).
    #[must_use]
    pub fn from_filetime(t: FileTime) -> Self {
        Self(t.to_unix_secs())
    }

    /// Whole days since 1970-01-01 (negative before).
    #[must_use]
    pub const fn days_since_epoch(self) -> i64 {
        self.0.div_euclid(SECS_PER_DAY)
    }

    /// Index of the ISO week (Monday 00:00 UTC through Sunday 23:59:59 UTC)
    /// containing this instant. Consecutive weeks have consecutive indices.
    ///
    /// Only equality of indices is meaningful; this is not the ISO week
    /// number. Grouping by it matches ISO-week grouping exactly, including
    /// weeks that straddle a year boundary.
    #[must_use]
    pub const fn iso_week_index(self) -> i64 {
        // 1970-01-01 was a Thursday, so shifting by 3 days puts every Monday
        // on a multiple of 7.
        (self.days_since_epoch() + 3).div_euclid(7)
    }

    /// Start of the UTC hour containing this instant (saturating at the
    /// bottom of the range, whose hour starts before `i64::MIN`).
    #[must_use]
    pub const fn hour_start(self) -> Self {
        Self(self.0.saturating_sub(self.0.rem_euclid(SECS_PER_HOUR)))
    }

    /// This instant minus `days` whole days, saturating at the range ends.
    #[must_use]
    pub const fn minus_days(self, days: u32) -> Self {
        Self(self.0.saturating_sub(days as i64 * SECS_PER_DAY))
    }

    /// Splits into a UTC calendar date and time.
    #[must_use]
    pub fn to_civil(self) -> CivilUtc {
        let (year, month, day) = civil_from_days(self.days_since_epoch());
        // rem_euclid keeps pre-1970 instants on the right side of midnight.
        let secs = self.0.rem_euclid(SECS_PER_DAY) as u32;
        CivilUtc {
            year,
            month,
            day,
            hour: secs / 3600,
            minute: secs / 60 % 60,
            second: secs % 60,
        }
    }

    /// Compact form used in file names: `20261009T133000Z`.
    #[must_use]
    pub fn to_compact_string(self) -> String {
        let c = self.to_civil();
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            c.year, c.month, c.day, c.hour, c.minute, c.second
        )
    }
}

impl fmt::Display for Timestamp {
    /// RFC 3339 in UTC, e.g. `2026-10-09T13:30:00Z`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.to_civil();
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            c.year, c.month, c.day, c.hour, c.minute, c.second
        )
    }
}

const fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`: shifts the year to start in March so
/// the leap day is last, then counts 400-year eras.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Source of "now" for every time-dependent store operation.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current UTC instant.
    fn now(&self) -> Timestamp;
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => Timestamp(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            Err(e) => Timestamp(-i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX)),
        }
    }
}

/// A clock that only moves when told to. Used to run retention and other
/// time-dependent logic against synthetic dates.
///
/// # Example
///
/// ```
/// use strata_store::{Clock, ManualClock, Timestamp};
/// let clock = ManualClock::new(Timestamp(1_000));
/// clock.advance_secs(60);
/// assert_eq!(clock.now(), Timestamp(1_060));
/// ```
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// A clock frozen at `start`.
    #[must_use]
    pub fn new(start: Timestamp) -> Self {
        Self(AtomicI64::new(start.0))
    }

    /// Jumps to `t` (backwards jumps are allowed, as real clocks do that too).
    pub fn set(&self, t: Timestamp) {
        self.0.store(t.0, Ordering::SeqCst);
    }

    /// Moves forward by `secs` seconds (negative moves backward).
    pub fn advance_secs(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_across_eras() {
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn known_dates() {
        assert_eq!(Timestamp::from_utc(1970, 1, 1, 0, 0, 0), Some(Timestamp(0)));
        assert_eq!(
            Timestamp::from_utc(2000, 2, 29, 0, 0, 0),
            Some(Timestamp(951_782_400))
        );
        assert_eq!(Timestamp::from_utc(2001, 2, 29, 0, 0, 0), None);
        assert_eq!(Timestamp::from_utc(2026, 13, 1, 0, 0, 0), None);
        assert_eq!(Timestamp(-1).to_string(), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn iso_weeks_start_on_monday_utc() {
        let sun = Timestamp::from_utc(2026, 10, 11, 23, 59, 59).unwrap();
        let mon = Timestamp::from_utc(2026, 10, 12, 0, 0, 0).unwrap();
        let prev_mon = Timestamp::from_utc(2026, 10, 5, 0, 0, 0).unwrap();
        assert_eq!(sun.iso_week_index() + 1, mon.iso_week_index());
        assert_eq!(prev_mon.iso_week_index(), sun.iso_week_index());
    }

    #[test]
    fn iso_week_straddles_year_end() {
        // 2026-W53 runs Monday 2026-12-28 through Sunday 2027-01-03.
        let a = Timestamp::from_utc(2026, 12, 28, 0, 0, 0).unwrap();
        let b = Timestamp::from_utc(2027, 1, 3, 23, 0, 0).unwrap();
        let c = Timestamp::from_utc(2027, 1, 4, 0, 0, 0).unwrap();
        assert_eq!(a.iso_week_index(), b.iso_week_index());
        assert_ne!(b.iso_week_index(), c.iso_week_index());
    }

    #[test]
    fn hour_start_handles_negative_times() {
        assert_eq!(Timestamp(-1).hour_start(), Timestamp(-3600));
        assert_eq!(Timestamp(7_199).hour_start(), Timestamp(3_600));
    }

    #[test]
    fn manual_clock_moves_only_on_request() {
        let c = ManualClock::new(Timestamp(10));
        assert_eq!(c.now(), Timestamp(10));
        c.advance_secs(-20);
        assert_eq!(c.now(), Timestamp(-10));
        c.set(Timestamp(5));
        assert_eq!(c.now(), Timestamp(5));
    }

    #[test]
    fn compact_string_is_filename_safe() {
        let t = Timestamp::from_utc(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(t.to_compact_string(), "20260102T030405Z");
    }
}
