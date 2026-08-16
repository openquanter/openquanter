//! UTC day arithmetic.
//!
//! A capture file holds exactly one UTC day, decided by the exchange
//! timestamp of each record. Doing this with the host's local timezone
//! would make the archive's meaning depend on a setting nobody records,
//! which is a trap for whoever reads the data years later.

/// Nanoseconds in a day.
const NS_PER_DAY: i64 = 86_400_000_000_000;

/// A UTC calendar day, counted from 1970-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UtcDay(pub i64);

impl UtcDay {
    /// The day containing a nanosecond timestamp.
    ///
    /// Uses floor division so timestamps before the epoch land on the
    /// day that contains them rather than the one after.
    #[must_use]
    pub fn from_nanos(ts: i64) -> Self {
        Self(ts.div_euclid(NS_PER_DAY))
    }

    /// First nanosecond of this day.
    #[must_use]
    pub fn start_nanos(self) -> i64 {
        self.0 * NS_PER_DAY
    }

    /// Calendar date as `(year, month, day)`.
    ///
    /// Howard Hinnant's `civil_from_days`, which is exact for the whole
    /// proleptic Gregorian range.
    #[must_use]
    pub fn to_ymd(self) -> (i64, u32, u32) {
        let z = self.0 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
}

impl core::fmt::Display for UtcDay {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (y, m, d) = self.to_ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(UtcDay(0).to_string(), "1970-01-01");
        assert_eq!(UtcDay(18_262).to_string(), "2020-01-01");
        assert_eq!(UtcDay(11_017).to_string(), "2000-03-01");
        assert_eq!(UtcDay(-1).to_string(), "1969-12-31");
    }

    #[test]
    fn leap_days_exist_where_they_should() {
        let leap = UtcDay(18_262 - 1); // 2019-12-31
        assert_eq!(leap.to_string(), "2019-12-31");
        // 2020 was a leap year: 2020-02-29 must exist and be followed by
        // 2020-03-01.
        let feb29 = UtcDay(18_262 + 31 + 28);
        assert_eq!(feb29.to_string(), "2020-02-29");
        assert_eq!(UtcDay(feb29.0 + 1).to_string(), "2020-03-01");
    }

    #[test]
    fn boundaries_are_exact() {
        let day = UtcDay(20_000);
        let start = day.start_nanos();
        assert_eq!(UtcDay::from_nanos(start), day);
        assert_eq!(UtcDay::from_nanos(start + NS_PER_DAY - 1), day);
        assert_eq!(UtcDay::from_nanos(start + NS_PER_DAY), UtcDay(20_001));
        assert_eq!(UtcDay::from_nanos(start - 1), UtcDay(19_999));
    }

    #[test]
    fn round_trips_over_a_long_range() {
        // Every day for forty years, formatted and re-derived.
        for day in 0..14_610 {
            let d = UtcDay(day);
            assert_eq!(UtcDay::from_nanos(d.start_nanos()), d);
            let (y, m, dd) = d.to_ymd();
            assert!((1970..=2010).contains(&y), "year {y} out of range");
            assert!((1..=12).contains(&m));
            assert!((1..=31).contains(&dd));
        }
    }
}

/// How often the writer starts a new file.
///
/// A day is the natural archival unit and stays the default. An hour is
/// for capture hosts whose disk cannot hold two days of raw data: the
/// current file is being appended to and therefore cannot be
/// compressed, so the local peak is always about two rotation periods.
/// Shortening the period shortens the peak; it changes nothing else.
///
/// None of the format's guarantees depend on the period — payloads are
/// still verbatim, timestamps still dual, files still sealed with a
/// manifest and a hash, gaps still marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// One file per UTC day.
    Daily,
    /// One file per UTC hour, grouped in a directory per day.
    Hourly,
}

impl Rotation {
    /// Parse `daily` or `hourly`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "daily" | "day" => Some(Self::Daily),
            "hourly" | "hour" => Some(Self::Hourly),
            _ => None,
        }
    }
}

/// Nanoseconds in an hour.
const NS_PER_HOUR: i64 = 3_600_000_000_000;

/// The stretch of time one file covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Window {
    /// The UTC day it falls in.
    pub day: UtcDay,
    /// The hour within that day, when rotating hourly.
    pub hour: Option<u32>,
}

impl Window {
    /// The window containing `ts` under `rotation`.
    #[must_use]
    pub fn from_nanos(ts: i64, rotation: Rotation) -> Self {
        let day = UtcDay::from_nanos(ts);
        let hour = match rotation {
            Rotation::Daily => None,
            Rotation::Hourly => {
                let into_day = ts - day.start_nanos();
                Some(u32::try_from(into_day / NS_PER_HOUR).unwrap_or(0))
            }
        };
        Self { day, hour }
    }

    /// First nanosecond of the window.
    #[must_use]
    pub fn start_nanos(self) -> i64 {
        self.day.start_nanos() + i64::from(self.hour.unwrap_or(0)) * NS_PER_HOUR
    }

    /// Path of a file for this window, relative to the stream
    /// directory: `2026-08-16.oqcap` daily, `2026-08-16/05.oqcap`
    /// hourly.
    #[must_use]
    pub fn relative_path(self, extension: &str) -> std::path::PathBuf {
        match self.hour {
            None => std::path::PathBuf::from(format!("{}.{extension}", self.day)),
            Some(h) => {
                std::path::PathBuf::from(self.day.to_string()).join(format!("{h:02}.{extension}"))
            }
        }
    }
}

impl core::fmt::Display for Window {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.hour {
            None => write!(f, "{}", self.day),
            Some(h) => write!(f, "{}T{h:02}", self.day),
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    const DAY: i64 = 86_400_000_000_000;

    #[test]
    fn daily_windows_ignore_the_hour() {
        let noon = 20_000 * DAY + 12 * NS_PER_HOUR;
        let w = Window::from_nanos(noon, Rotation::Daily);
        assert_eq!(w.hour, None);
        assert_eq!(w.start_nanos(), 20_000 * DAY);
        assert_eq!(
            w.relative_path("oqcap").to_string_lossy(),
            "2024-10-04.oqcap"
        );
    }

    #[test]
    fn hourly_windows_split_the_day() {
        let base = 20_000 * DAY;
        for hour in [0u32, 5, 23] {
            let w = Window::from_nanos(base + i64::from(hour) * NS_PER_HOUR + 1, Rotation::Hourly);
            assert_eq!(w.hour, Some(hour));
            assert_eq!(
                w.relative_path("oqcap").to_string_lossy(),
                format!("2024-10-04/{hour:02}.oqcap")
            );
        }
    }

    #[test]
    fn windows_are_ordered_and_boundaries_are_exact() {
        let base = 20_000 * DAY;
        let last_ns_of_hour_4 = base + 5 * NS_PER_HOUR - 1;
        let first_ns_of_hour_5 = base + 5 * NS_PER_HOUR;
        let a = Window::from_nanos(last_ns_of_hour_4, Rotation::Hourly);
        let b = Window::from_nanos(first_ns_of_hour_5, Rotation::Hourly);
        assert_eq!((a.hour, b.hour), (Some(4), Some(5)));
        assert!(a < b, "windows must order so late records can be detected");
        assert_eq!(b.start_nanos(), first_ns_of_hour_5);
    }

    #[test]
    fn the_last_hour_of_a_day_precedes_the_first_of_the_next() {
        let a = Window::from_nanos(20_000 * DAY + 23 * NS_PER_HOUR, Rotation::Hourly);
        let b = Window::from_nanos(20_001 * DAY, Rotation::Hourly);
        assert!(a < b);
        assert_eq!(a.to_string(), "2024-10-04T23");
        assert_eq!(b.to_string(), "2024-10-05T00");
    }

    #[test]
    fn parses_its_own_names() {
        assert_eq!(Rotation::parse("hourly"), Some(Rotation::Hourly));
        assert_eq!(Rotation::parse("daily"), Some(Rotation::Daily));
        assert_eq!(Rotation::parse("weekly"), None);
    }
}
