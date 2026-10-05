//! UTC timestamp formatting and parsing compatible with Python's
//! `datetime.isoformat()` and `datetime.fromisoformat()` for the shapes this
//! tool reads and writes.

use std::time::{SystemTime, UNIX_EPOCH};

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// Microseconds since the Unix epoch, UTC.
pub type Micros = i64;

pub fn now_micros() -> Micros {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_micros() as i64,
        Err(error) => -(error.duration().as_micros() as i64),
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `datetime.fromtimestamp(t, UTC).isoformat()` for an exact microsecond
/// instant. Returns `None` outside Python's supported year range.
pub fn isoformat_utc(micros: Micros) -> Option<String> {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let seconds = rem / MICROS_PER_SECOND;
    let fraction = rem % MICROS_PER_SECOND;
    let mut text = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    );
    if fraction != 0 {
        text.push_str(&format!(".{fraction:06}"));
    }
    text.push_str("+00:00");
    Some(text)
}

fn round_half_even(value: f64) -> f64 {
    let rounded = value.round();
    if (value - rounded).abs() == 0.5 {
        2.0 * (value / 2.0).round()
    } else {
        rounded
    }
}

/// Convert a `stat` modification time to microseconds exactly the way Python
/// does: `st_mtime` is the double `sec + nsec * 1e-9`, and
/// `datetime.fromtimestamp` rounds it half-to-even to microseconds.
pub fn mtime_micros(sec: i64, nsec: i64) -> Micros {
    let timestamp = sec as f64 + nsec as f64 * 1e-9;
    let mut int_part = timestamp.trunc();
    let mut frac = round_half_even((timestamp - int_part) * 1e6);
    if frac >= 1e6 {
        frac -= 1e6;
        int_part += 1.0;
    } else if frac < 0.0 {
        frac += 1e6;
        int_part -= 1.0;
    }
    int_part as i64 * MICROS_PER_SECOND + frac as i64
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, count: usize) -> Option<i64> {
        let slice = self.bytes.get(self.pos..self.pos + count)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.pos += count;
        Some(
            slice
                .iter()
                .fold(0i64, |acc, digit| acc * 10 + i64::from(digit - b'0')),
        )
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// Parse `HH[:MM[:SS[.f]]]` (or the basic `HHMMSS` spelling). Returns the
/// time of day in microseconds.
fn parse_clock(cursor: &mut Cursor<'_>, allow_fraction: bool) -> Option<i64> {
    let hour = cursor.digits(2)?;
    let mut minute = 0;
    let mut second = 0;
    let mut micros = 0;
    let extended = cursor.peek() == Some(b':');
    let component = |cursor: &mut Cursor<'_>| -> Option<Option<i64>> {
        if extended {
            if !cursor.eat(b':') {
                return Some(None);
            }
            cursor.digits(2).map(Some)
        } else if cursor.peek().is_some_and(|b| b.is_ascii_digit()) {
            cursor.digits(2).map(Some)
        } else {
            Some(None)
        }
    };
    if let Some(value) = component(cursor)? {
        minute = value;
        if let Some(value) = component(cursor)? {
            second = value;
            if allow_fraction && (cursor.peek() == Some(b'.') || cursor.peek() == Some(b',')) {
                cursor.pos += 1;
                let start = cursor.pos;
                while cursor.peek().is_some_and(|b| b.is_ascii_digit()) {
                    cursor.pos += 1;
                }
                let fraction = &cursor.bytes[start..cursor.pos];
                if fraction.is_empty() {
                    return None;
                }
                let mut padded = [b'0'; 6];
                for (slot, digit) in padded.iter_mut().zip(fraction.iter()) {
                    *slot = *digit;
                }
                micros = padded
                    .iter()
                    .fold(0i64, |acc, digit| acc * 10 + i64::from(digit - b'0'));
            }
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + micros)
}

/// `datetime.fromisoformat(value.replace("Z", "+00:00"))` converted to UTC
/// microseconds (naive values are treated as UTC). Returns `None` where
/// Python raises `ValueError`.
pub fn parse_iso(value: &str) -> Option<Micros> {
    let replaced = value.replace('Z', "+00:00");
    let mut cursor = Cursor {
        bytes: replaced.as_bytes(),
        pos: 0,
    };
    let year = cursor.digits(4)?;
    let extended = cursor.eat(b'-');
    let month = cursor.digits(2)?;
    if extended && !cursor.eat(b'-') {
        return None;
    }
    let day = cursor.digits(2)?;
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut micros = days_from_civil(year, month, day) * MICROS_PER_DAY;
    if cursor.done() {
        return Some(micros);
    }
    // Any single character may separate the date and the time.
    let rest = std::str::from_utf8(&cursor.bytes[cursor.pos..]).ok()?;
    let separator = rest.chars().next()?;
    cursor.pos += separator.len_utf8();
    micros += parse_clock(&mut cursor, true)?;
    if cursor.done() {
        return Some(micros);
    }
    let sign = match cursor.peek()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    cursor.pos += 1;
    let offset = parse_clock(&mut cursor, true)?;
    if !cursor.done() {
        return None;
    }
    micros -= sign * offset;
    Some(micros)
}

/// `max(0, (now - parsed).days)` or `None` when the value does not parse.
pub fn age_days(value: &str, now: Micros) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    let parsed = parse_iso(value)?;
    Some(((now - parsed).div_euclid(MICROS_PER_DAY)).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isoformat_matches_python() {
        assert_eq!(
            isoformat_utc(0).as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        assert_eq!(
            isoformat_utc(1_767_225_600_000_000 + 123_456).as_deref(),
            Some("2026-01-01T00:00:00.123456+00:00")
        );
        assert_eq!(
            isoformat_utc(-1).as_deref(),
            Some("1969-12-31T23:59:59.999999+00:00")
        );
    }

    #[test]
    fn mtime_rounding_matches_python() {
        // datetime.fromtimestamp(1700000000.1234565, UTC) -> .123456 or .123457
        // depending on binary representation; spot-check exact values.
        assert_eq!(mtime_micros(1_700_000_000, 0), 1_700_000_000_000_000);
        assert_eq!(
            mtime_micros(1_700_000_000, 999_999_999),
            1_700_000_001_000_000
        );
        assert_eq!(mtime_micros(1_700_000_000, 500_000), 1_700_000_000_000_500);
    }

    #[test]
    fn parse_iso_handles_common_shapes() {
        let base = parse_iso("2026-01-01T00:00:00+00:00").unwrap();
        assert_eq!(parse_iso("2026-01-01T00:00:00Z"), Some(base));
        assert_eq!(parse_iso("2026-01-01T02:00:00+02:00"), Some(base));
        assert_eq!(parse_iso("2026-01-01"), Some(base));
        assert_eq!(
            parse_iso("2026-01-01T00:00:00.5+00:00"),
            Some(base + 500_000)
        );
        assert_eq!(parse_iso("20260101T000000"), Some(base));
        assert_eq!(parse_iso("not-a-date"), None);
        assert_eq!(parse_iso("2026-02-30"), None);
    }

    #[test]
    fn age_days_floors_and_clamps() {
        let now = parse_iso("2026-01-01T00:00:00+00:00").unwrap();
        assert_eq!(age_days("2025-01-01T00:00:00+00:00", now), Some(365));
        assert_eq!(age_days("2027-01-01T00:00:00+00:00", now), Some(0));
        assert_eq!(age_days("", now), None);
        assert_eq!(age_days("garbage", now), None);
    }
}
