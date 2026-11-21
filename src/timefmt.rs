//! UTC timestamp helpers built on the `time` crate.
//!
//! CipherPay stores timestamps as TEXT and compares many of them lexically in
//! SQL, so every string produced here must be byte-for-byte what the previous
//! chrono code produced. The parsers accept what chrono's parsers accepted for
//! the inputs this service sees. The tests below are golden values captured
//! from chrono 0.4.45.

use std::fmt;

use time::{Date, Month, Time, UtcOffset};
pub use time::{Duration, OffsetDateTime, PrimitiveDateTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampError(&'static str);

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid timestamp: {}", self.0)
    }
}

impl std::error::Error for TimestampError {}

pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn utc(t: OffsetDateTime) -> OffsetDateTime {
    t.to_offset(UtcOffset::UTC)
}

fn date_time(p: PrimitiveDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        p.year(),
        u8::from(p.month()),
        p.day(),
        p.hour(),
        p.minute(),
        p.second()
    )
}

fn primitive(t: OffsetDateTime) -> PrimitiveDateTime {
    let t = utc(t);
    PrimitiveDateTime::new(t.date(), t.time())
}

/// `%Y-%m-%dT%H:%M:%SZ` in UTC, truncated to whole seconds.
pub fn format_z(t: OffsetDateTime) -> String {
    format!("{}Z", date_time(primitive(t)))
}

/// `%Y-%m-%dT%H:%M:%S` in UTC, without an offset.
pub fn format_naive(t: OffsetDateTime) -> String {
    date_time(primitive(t))
}

/// `%Y-%m-%dT%H:%M`.
pub fn format_naive_minutes(p: PrimitiveDateTime) -> String {
    let s = date_time(p);
    s[..s.len() - 3].to_string()
}

/// Fractional seconds the way chrono's `SecondsFormat::AutoSi` writes them.
fn auto_si_fraction(nanos: u32) -> String {
    if nanos == 0 {
        String::new()
    } else if nanos % 1_000_000 == 0 {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    }
}

/// chrono's `DateTime<Utc>::to_rfc3339()`: AutoSi fraction and `+00:00`.
pub fn rfc3339(t: OffsetDateTime) -> String {
    let t = utc(t);
    format!(
        "{}{}+00:00",
        date_time(primitive(t)),
        auto_si_fraction(t.nanosecond())
    )
}

/// chrono's `to_rfc3339_opts(SecondsFormat::Secs, true)`.
pub fn rfc3339_secs_z(t: OffsetDateTime) -> String {
    format_z(t)
}

/// chrono's `to_rfc3339_opts(SecondsFormat::Millis, true)`.
pub fn rfc3339_millis_z(t: OffsetDateTime) -> String {
    let t = utc(t);
    format!(
        "{}.{:03}Z",
        date_time(primitive(t)),
        t.nanosecond() / 1_000_000
    )
}

/// Serialize like chrono's serde impl for `DateTime<Utc>` (AutoSi, `Z`).
pub fn serialize_rfc3339_z<S: serde::Serializer>(
    t: &OffsetDateTime,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let t = utc(*t);
    serializer.serialize_str(&format!(
        "{}{}Z",
        date_time(primitive(t)),
        auto_si_fraction(t.nanosecond())
    ))
}

/// Method-style access so call sites read like the chrono code they replace.
pub trait TimestampExt {
    fn format_z(self) -> String;
    fn format_naive(self) -> String;
    fn to_rfc3339(self) -> String;
    fn rfc3339_secs_z(self) -> String;
    fn rfc3339_millis_z(self) -> String;
}

impl TimestampExt for OffsetDateTime {
    fn format_z(self) -> String {
        format_z(self)
    }
    fn format_naive(self) -> String {
        format_naive(self)
    }
    fn to_rfc3339(self) -> String {
        rfc3339(self)
    }
    fn rfc3339_secs_z(self) -> String {
        rfc3339_secs_z(self)
    }
    fn rfc3339_millis_z(self) -> String {
        rfc3339_millis_z(self)
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    /// Between `min` and `max` ASCII digits.
    fn digits(&mut self, min: usize, max: usize) -> Result<u32, TimestampError> {
        let start = self.i;
        let mut v: u32 = 0;
        while self.i < self.b.len() && self.i - start < max && self.b[self.i].is_ascii_digit() {
            v = v * 10 + u32::from(self.b[self.i] - b'0');
            self.i += 1;
        }
        if self.i - start < min {
            return Err(TimestampError("expected digits"));
        }
        Ok(v)
    }

    fn lit(&mut self, options: &[u8]) -> Result<u8, TimestampError> {
        match self.b.get(self.i) {
            Some(c) if options.contains(c) => {
                self.i += 1;
                Ok(*c)
            }
            _ => Err(TimestampError("unexpected character")),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn done(&self) -> Result<(), TimestampError> {
        if self.i == self.b.len() {
            Ok(())
        } else {
            Err(TimestampError("trailing input"))
        }
    }
}

fn build(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    nanos: u32,
) -> Result<PrimitiveDateTime, TimestampError> {
    let out_of_range = |_| TimestampError("out of range");
    let month = Month::try_from(u8::try_from(month).map_err(|_| TimestampError("out of range"))?)
        .map_err(out_of_range)?;
    let date = Date::from_calendar_date(year as i32, month, day as u8).map_err(out_of_range)?;
    // chrono represents a leap second as :59 plus an extra second of nanos; the
    // nearest instant `time` can hold is the end of second 59.
    let (second, nanos) = if second == 60 {
        (59, 999_999_999)
    } else {
        (second, nanos)
    };
    let time =
        Time::from_hms_nano(hour as u8, minute as u8, second as u8, nanos).map_err(out_of_range)?;
    Ok(PrimitiveDateTime::new(date, time))
}

/// chrono's `DateTime::parse_from_rfc3339`: `T`, `t` or space separator,
/// any number of fraction digits (beyond nine are ignored), `Z`/`z` or a
/// `±HH:MM` offset below 24 hours.
pub fn parse_rfc3339(s: &str) -> Result<OffsetDateTime, TimestampError> {
    let mut c = Cursor::new(s);
    let year = c.digits(4, 4)?;
    c.lit(b"-")?;
    let month = c.digits(2, 2)?;
    c.lit(b"-")?;
    let day = c.digits(2, 2)?;
    c.lit(b"Tt ")?;
    let hour = c.digits(2, 2)?;
    c.lit(b":")?;
    let minute = c.digits(2, 2)?;
    c.lit(b":")?;
    let second = c.digits(2, 2)?;
    let mut nanos = 0u32;
    if c.peek() == Some(b'.') {
        c.i += 1;
        let start = c.i;
        while let Some(d) = c.peek().filter(u8::is_ascii_digit) {
            if c.i - start < 9 {
                nanos = nanos * 10 + u32::from(d - b'0');
            }
            c.i += 1;
        }
        let n = c.i - start;
        if n == 0 {
            return Err(TimestampError("empty fraction"));
        }
        for _ in n..9 {
            nanos *= 10;
        }
    }
    let offset_secs: i32 = match c.lit(b"Zz+-")? {
        b'Z' | b'z' => 0,
        sign => {
            let h = c.digits(2, 2)? as i32;
            c.lit(b":")?;
            let m = c.digits(2, 2)? as i32;
            if h > 23 || m > 59 {
                return Err(TimestampError("offset out of range"));
            }
            let v = h * 3600 + m * 60;
            if sign == b'-' {
                -v
            } else {
                v
            }
        }
    };
    c.done()?;
    let p = build(year, month, day, hour, minute, second, nanos)?;
    let offset = UtcOffset::from_whole_seconds(offset_secs)
        .map_err(|_| TimestampError("offset out of range"))?;
    Ok(utc(p.assume_offset(offset)))
}

/// Numeric fields as chrono's `parse_from_str` reads `%Y-%m-%dT%H:%M[:%S]`:
/// a four-digit year and one- or two-digit other fields.
fn parse_fields(c: &mut Cursor<'_>, seconds: bool) -> Result<PrimitiveDateTime, TimestampError> {
    let year = c.digits(4, 4)?;
    c.lit(b"-")?;
    let month = c.digits(1, 2)?;
    c.lit(b"-")?;
    let day = c.digits(1, 2)?;
    c.lit(b"T")?;
    let hour = c.digits(1, 2)?;
    c.lit(b":")?;
    let minute = c.digits(1, 2)?;
    let second = if seconds {
        c.lit(b":")?;
        c.digits(1, 2)?
    } else {
        0
    };
    build(year, month, day, hour, minute, second, 0)
}

/// chrono's `NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")`, as UTC.
pub fn parse_z(s: &str) -> Result<OffsetDateTime, TimestampError> {
    let mut c = Cursor::new(s);
    let p = parse_fields(&mut c, true)?;
    c.lit(b"Z")?;
    c.done()?;
    Ok(p.assume_utc())
}

/// `%Y-%m-%dT%H:%M:%S`, falling back to `%Y-%m-%dT%H:%M`.
pub fn parse_naive(s: &str) -> Result<PrimitiveDateTime, TimestampError> {
    let mut c = Cursor::new(s);
    if let Ok(p) = parse_fields(&mut c, true).and_then(|p| c.done().map(|_| p)) {
        return Ok(p);
    }
    let mut c = Cursor::new(s);
    let p = parse_fields(&mut c, false)?;
    c.done()?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64, nanos: u32) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).unwrap() + Duration::nanoseconds(nanos as i64)
    }

    fn ts(t: OffsetDateTime) -> (i64, u32) {
        (t.unix_timestamp(), t.nanosecond())
    }

    #[test]
    fn formats_match_chrono() {
        // (secs, nanos, to_rfc3339, Secs+Z, Millis+Z, serde, %..%SZ, naive, minutes)
        let cases = [
            (
                0,
                "2025-11-21T04:40:00+00:00",
                "2025-11-21T04:40:00.000Z",
                "\"2025-11-21T04:40:00Z\"",
            ),
            (
                123_000_000,
                "2025-11-21T04:40:00.123+00:00",
                "2025-11-21T04:40:00.123Z",
                "\"2025-11-21T04:40:00.123Z\"",
            ),
            (
                123_456_000,
                "2025-11-21T04:40:00.123456+00:00",
                "2025-11-21T04:40:00.123Z",
                "\"2025-11-21T04:40:00.123456Z\"",
            ),
            (
                123_456_789,
                "2025-11-21T04:40:00.123456789+00:00",
                "2025-11-21T04:40:00.123Z",
                "\"2025-11-21T04:40:00.123456789Z\"",
            ),
            (
                5_000,
                "2025-11-21T04:40:00.000005+00:00",
                "2025-11-21T04:40:00.000Z",
                "\"2025-11-21T04:40:00.000005Z\"",
            ),
        ];
        #[derive(serde::Serialize)]
        struct W(#[serde(serialize_with = "serialize_rfc3339_z")] OffsetDateTime);
        for (nanos, rfc, millis, serde) in cases {
            let t = at(1_763_700_000, nanos);
            assert_eq!(rfc3339(t), rfc);
            assert_eq!(rfc3339_secs_z(t), "2025-11-21T04:40:00Z");
            assert_eq!(rfc3339_millis_z(t), millis);
            assert_eq!(serde_json::to_string(&W(t)).unwrap(), serde);
            assert_eq!(format_z(t), "2025-11-21T04:40:00Z");
            assert_eq!(format_naive(t), "2025-11-21T04:40:00");
            assert_eq!(format_naive_minutes(primitive(t)), "2025-11-21T04:40");
        }
        // Non-UTC offsets are written in UTC.
        let tokyo = at(1_763_700_000, 0).to_offset(UtcOffset::from_hms(9, 0, 0).unwrap());
        assert_eq!(format_z(tokyo), "2025-11-21T04:40:00Z");
        assert_eq!(rfc3339(tokyo), "2025-11-21T04:40:00+00:00");
    }

    #[test]
    fn rfc3339_parse_matches_chrono() {
        let ok = [
            ("2026-11-21T04:40:00Z", (1_795_236_000, 0)),
            ("2026-11-21t04:40:00z", (1_795_236_000, 0)),
            ("2026-11-21 04:40:00Z", (1_795_236_000, 0)),
            (
                "2026-11-21T04:40:00.1234567891234Z",
                (1_795_236_000, 123_456_789),
            ),
            ("2026-11-21T04:40:00.000000000000Z", (1_795_236_000, 0)),
            ("2026-11-21T13:40:00+09:00", (1_795_236_000, 0)),
            ("2026-11-21T04:40:00-00:00", (1_795_236_000, 0)),
            ("2026-11-21T04:40:00.5-00:30", (1_795_237_800, 500_000_000)),
        ];
        for (s, want) in ok {
            assert_eq!(ts(parse_rfc3339(s).unwrap()), want, "{s}");
        }
        let err = [
            "2026-11-21T04:40:00",
            "2026-11-21T04:40:00+24:00",
            "2026-02-30T00:00:00Z",
            "2026-11-21T04:40:00.Z",
            "2026-11-21T04:40:00+0900",
            "2026-11-21T04:40:00+09",
            "2026-1-21T04:40:00Z",
            "2026-11-21T4:40:00Z",
            "2026-11-21T04:40:00+9:00",
            "2026-11-21T04:40:00+09:00:00",
            "2026-11-21T04:40:00Z ",
            "2026-11-21  04:40:00Z",
        ];
        for s in err {
            assert!(parse_rfc3339(s).is_err(), "{s}");
        }
        // chrono keeps a leap second as 59 + 1e9 ns; we hold the end of :59.
        assert_eq!(
            ts(parse_rfc3339("2026-11-21T04:40:60Z").unwrap()),
            (1_795_236_059, 999_999_999)
        );
    }

    #[test]
    fn z_and_naive_parse_match_chrono() {
        for (s, want) in [
            ("2026-11-21T04:40:00Z", 1_795_236_000),
            ("2026-11-21T4:40:00Z", 1_795_236_000),
            ("2026-1-5T04:40:00Z", 1_767_588_000),
        ] {
            assert_eq!(parse_z(s).unwrap().unix_timestamp(), want, "{s}");
        }
        for s in [
            "2026-11-21T04:40:00",
            "2026-11-21T04:40:00.5Z",
            "2026-11-21t04:40:00Z",
            "2026-11-21T04:40:00z",
            "2026-11-21T04:40:00Z ",
            "2026-11-21T004:40:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-21T24:00:00Z",
            "12026-11-21T04:40:00Z",
        ] {
            assert!(parse_z(s).is_err(), "{s}");
        }
        for (s, want) in [
            ("2026-11-21T04:40:00", 1_795_236_000),
            ("2026-11-21T04:40", 1_795_236_000),
            ("2026-11-21T4:40", 1_795_236_000),
            ("2026-1-5T04:40", 1_767_588_000),
            ("2026-11-21T04:40:5", 1_795_236_005),
            ("2026-11-21T04:4", 1_795_233_840),
        ] {
            assert_eq!(
                parse_naive(s).unwrap().assume_utc().unix_timestamp(),
                want,
                "{s}"
            );
        }
        for s in [
            "2026-11-21T04:40:00Z",
            "2026-11-21 04:40",
            "2026-11-21t04:40",
        ] {
            assert!(parse_naive(s).is_err(), "{s}");
        }
    }
}
