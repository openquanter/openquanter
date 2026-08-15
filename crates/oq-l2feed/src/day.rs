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
