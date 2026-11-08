//! Wall-clock timestamps and durations, both stored in the registry as
//! human-readable strings (`2026-11-08T18:40:00.000Z`, `24h`, `1m30s`).
//!
//! Everything that decides *when* something happens takes a `Ts` argument
//! instead of reading the clock itself, so state transitions are testable
//! with any clock.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ts(pub u64);

/// A duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Dur(pub u64);

impl Ts {
    /// 9999-12-31T23:59:59.999Z, the last instant the four-digit-year
    /// format can hold. Times are clamped to it so the registry always reads
    /// back, however large a delay is.
    pub const MAX: Ts = Ts(253_402_300_799_999);

    pub fn now() -> Ts {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ts(ms)
    }

    pub fn plus(self, d: Dur) -> Ts {
        Ts(self.0.saturating_add(d.0).min(Ts::MAX.0))
    }

    /// `self - earlier`, zero if `earlier` is later.
    pub fn since(self, earlier: Ts) -> Dur {
        Dur(self.0.saturating_sub(earlier.0))
    }

    /// RFC 3339, UTC, millisecond precision.
    pub fn rfc3339(self) -> String {
        let secs = (self.0 / 1000) as i64;
        let ms = self.0 % 1000;
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        )
    }

    /// Like [`Ts::rfc3339`] but to the second, for terminal output.
    pub fn short(self) -> String {
        let mut s = self.rfc3339();
        s.replace_range(19..23, "");
        s
    }

    /// Parses the format written by [`Ts::rfc3339`] (millis optional).
    pub fn parse(s: &str) -> Result<Ts, String> {
        let bad = || format!("invalid timestamp {s:?} (want YYYY-MM-DDTHH:MM:SS[.mmm]Z)");
        let b = s.as_bytes();
        if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
            return Err(bad());
        }
        let num = |r: std::ops::Range<usize>| -> Result<u64, String> {
            s.get(r).and_then(|t| t.parse().ok()).ok_or_else(bad)
        };
        let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
        let ms = match &s[19..s.len() - 1] {
            "" => 0,
            frac if frac.len() == 4 && frac.starts_with('.') => num(20..23)?,
            _ => return Err(bad()),
        };
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
            return Err(bad());
        }
        let days = days_from_civil(y as i64, mo as u32, d as u32);
        if days < 0 {
            return Err(bad());
        }
        Ok(Ts(
            (days as u64 * 86_400 + h * 3600 + mi * 60 + se) * 1000 + ms
        ))
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rfc3339())
    }
}

impl Serialize for Ts {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.rfc3339())
    }
}

impl<'de> Deserialize<'de> for Ts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Ts, D::Error> {
        let s = String::deserialize(d)?;
        Ts::parse(&s).map_err(serde::de::Error::custom)
    }
}

const UNITS: [(&str, u64); 6] = [
    ("w", 7 * 86_400_000),
    ("d", 86_400_000),
    ("h", 3_600_000),
    ("m", 60_000),
    ("s", 1000),
    ("ms", 1),
];

impl Dur {
    pub const ZERO: Dur = Dur(0);

    pub fn from_std(d: Duration) -> Dur {
        Dur(d.as_millis() as u64)
    }

    pub fn std(self) -> Duration {
        Duration::from_millis(self.0)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Parses `30s`, `10m`, `24h`, `2d`, `1w`, `500ms` and sums like `1h30m`.
    /// A bare `0` is allowed; any other number needs a unit.
    pub fn parse(s: &str) -> Result<Dur, String> {
        let s = s.trim();
        if s == "0" {
            return Ok(Dur::ZERO);
        }
        let err =
            |why: &str| format!("invalid duration {s:?}: {why} (examples: 10s, 5m, 24h, 1h30m)");
        if s.is_empty() {
            return Err(err("empty"));
        }
        let mut total: u64 = 0;
        let mut rest = s;
        while !rest.is_empty() {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if digits == 0 {
                return Err(err("expected a number"));
            }
            let n: u64 = rest[..digits]
                .parse()
                .map_err(|_| err("number too large"))?;
            rest = &rest[digits..];
            let ulen = rest
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let unit = &rest[..ulen];
            if unit.is_empty() {
                return Err(err("missing unit"));
            }
            let (_, mult) = UNITS
                .iter()
                .find(|(u, _)| *u == unit)
                .ok_or_else(|| err(&format!("unknown unit {unit:?}")))?;
            let part = n.checked_mul(*mult).ok_or_else(|| err("too large"))?;
            total = total.checked_add(part).ok_or_else(|| err("too large"))?;
            rest = &rest[ulen..];
        }
        Ok(Dur(total))
    }
}

impl fmt::Display for Dur {
    /// The canonical form [`Dur::parse`] reads back: `1d2h`, `90s` → `1m30s`, `0`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0");
        }
        let mut left = self.0;
        for (unit, mult) in UNITS {
            if left >= mult {
                write!(f, "{}{unit}", left / mult)?;
                left %= mult;
            }
        }
        Ok(())
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Dur, D::Error> {
        let s = String::deserialize(d)?;
        Dur::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A coarse human form for terminal output: `3s`, `4m`, `2h5m`, `1d3h`.
pub fn approx(d: Dur) -> String {
    let ms = d.0;
    let rounded = if ms < 1000 {
        Dur(ms)
    } else if ms < 3_600_000 {
        Dur(ms / 1000 * 1000)
    } else if ms < 86_400_000 {
        Dur(ms / 60_000 * 60_000)
    } else {
        Dur(ms / 3_600_000 * 3_600_000)
    };
    rounded.to_string()
}

// Howard Hinnant's civil-date algorithms (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(Dur::parse("0"), Ok(Dur(0)));
        assert_eq!(Dur::parse("500ms"), Ok(Dur(500)));
        assert_eq!(Dur::parse("10s"), Ok(Dur(10_000)));
        assert_eq!(Dur::parse("5m"), Ok(Dur(300_000)));
        assert_eq!(Dur::parse("24h"), Ok(Dur(86_400_000)));
        assert_eq!(Dur::parse("2d"), Ok(Dur(172_800_000)));
        assert_eq!(Dur::parse("1w"), Ok(Dur(604_800_000)));
        assert_eq!(Dur::parse("1h30m"), Ok(Dur(5_400_000)));
        assert_eq!(Dur::parse(" 1m1s "), Ok(Dur(61_000)));
    }

    #[test]
    fn rejects_bad_durations() {
        for bad in [
            "",
            "10",
            "h",
            "5x",
            "1.5h",
            "-3s",
            "s10",
            "99999999999999999999s",
        ] {
            assert!(Dur::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn duration_display_round_trips() {
        for ms in [
            0,
            1,
            999,
            1000,
            61_000,
            5_400_000,
            86_400_000,
            90_061_001,
            604_800_000,
        ] {
            let d = Dur(ms);
            assert_eq!(Dur::parse(&d.to_string()), Ok(d), "{d}");
        }
        assert_eq!(Dur(90_000).to_string(), "1m30s");
        assert_eq!(Dur(86_400_000).to_string(), "1d");
    }

    #[test]
    fn approx_rounds_down_to_a_readable_unit() {
        assert_eq!(approx(Dur(250)), "250ms");
        assert_eq!(approx(Dur(61_500)), "1m1s");
        assert_eq!(approx(Dur(3_725_000)), "1h2m");
        assert_eq!(approx(Dur(90_061_001)), "1d1h");
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(Ts(0).rfc3339(), "1970-01-01T00:00:00.000Z");
        let t = Ts::parse("2026-11-08T18:40:05.123Z").unwrap();
        assert_eq!(t.rfc3339(), "2026-11-08T18:40:05.123Z");
        assert_eq!(t.short(), "2026-11-08T18:40:05Z");
        assert_eq!(
            Ts::parse("2024-02-29T00:00:00Z").unwrap().rfc3339(),
            "2024-02-29T00:00:00.000Z"
        );
        let now = Ts::now();
        assert_eq!(Ts::parse(&now.rfc3339()), Ok(now));
    }

    #[test]
    fn far_future_times_clamp_and_still_round_trip() {
        // Found by the state-machine property test: a huge --after made a due
        // time in year 504474608, which serialized but never parsed back.
        assert_eq!(Ts::MAX.rfc3339(), "9999-12-31T23:59:59.999Z");
        assert_eq!(Ts::parse("9999-12-31T23:59:59.999Z"), Ok(Ts::MAX));
        let far = Ts(1_000_000).plus(Dur(u64::MAX));
        assert_eq!(far, Ts::MAX);
        assert_eq!(Ts::parse(&far.rfc3339()), Ok(far));
        assert_eq!(Ts::MAX.plus(Dur(1)), Ts::MAX);
    }

    #[test]
    fn rejects_bad_timestamps() {
        for bad in [
            "",
            "2026-11-08",
            "2026-13-01T00:00:00Z",
            "2026-11-08T25:00:00Z",
            "2026-11-08T00:00:00",
        ] {
            assert!(Ts::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
