//! Text forms of literal values: dates, timestamps, UUIDs, bytes and escapes.

/// Microseconds in a day.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Return the days since 1970-01-01 of a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Return the proleptic Gregorian `(year, month, day)` of a day count since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`.
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

/// Return the number of days in `month` of `year`.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Render a day count as `YYYY-MM-DD` (a leading `-` for years before 0).
fn format_civil(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    if year < 0 {
        format!("-{:04}-{month:02}-{day:02}", -year)
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// Parse `YYYY-MM-DD` at the start of `text`; return the day count and the rest.
fn parse_civil(text: &str) -> Option<(i64, &str)> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let year_len = text.find('-')?;
    if !(4..=9).contains(&year_len) || !text[..year_len].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i64 = text[..year_len].parse().ok()?;
    let year = if negative { -year } else { year };
    let rest = &text[year_len..];
    let month = two_digits(rest.strip_prefix('-')?)?;
    let day = two_digits(rest.get(4..)?)?;
    if rest.as_bytes().get(3) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some((days_from_civil(year, month, day), &rest[6..]))
}

/// Parse the two ASCII digits at the start of `text`.
fn two_digits(text: &str) -> Option<i64> {
    let digits = text.get(..2)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Render a `date` (days since 1970-01-01) as `YYYY-MM-DD`.
pub(crate) fn format_date(days: i32) -> String {
    format_civil(i64::from(days))
}

/// Parse a `YYYY-MM-DD` date into days since 1970-01-01.
pub(crate) fn parse_date(text: &str) -> Option<i32> {
    let (days, rest) = parse_civil(text)?;
    if !rest.is_empty() {
        return None;
    }
    i32::try_from(days).ok()
}

/// Render a `timestamptz` (microseconds since the epoch) as `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`.
pub(crate) fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let (secs, fraction) = (of_day / 1_000_000, of_day % 1_000_000);
    let mut out = format!(
        "{}T{:02}:{:02}:{:02}",
        format_civil(days),
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    if fraction != 0 {
        out.push_str(&format!(".{fraction:06}"));
    }
    out.push('Z');
    out
}

/// Parse `YYYY-MM-DD[T ]HH:MM:SS[.f{1,6}]` followed by `Z` or `±HH:MM` into
/// microseconds since the epoch.
pub(crate) fn parse_timestamp(text: &str) -> Option<i64> {
    let (days, rest) = parse_civil(text)?;
    let rest = rest.strip_prefix(['T', ' '])?;
    let (hour, minute, second) = (
        two_digits(rest)?,
        two_digits(rest.get(3..)?)?,
        two_digits(rest.get(6..)?)?,
    );
    if rest.as_bytes()[2] != b':' || rest.as_bytes()[5] != b':' {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = &rest[8..];
    let mut fraction = 0i64;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 || len > 6 {
            return None;
        }
        fraction = after_dot[..len].parse::<i64>().ok()? * 10i64.pow(6 - len as u32);
        rest = &after_dot[len..];
    }
    let offset_secs = match rest {
        "Z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let (h, m) = (two_digits(&rest[1..])?, two_digits(&rest[4..])?);
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let secs = i128::from(days) * 86_400 + i128::from(hour * 3600 + minute * 60 + second)
        - i128::from(offset_secs);
    i64::try_from(secs * 1_000_000 + i128::from(fraction)).ok()
}

/// Render bytes as lowercase hex.
pub(crate) fn format_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse hex digits (either case) into bytes.
pub(crate) fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Render a UUID in the canonical lowercase hyphenated form.
pub(crate) fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = format_hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Parse a hyphenated UUID (either case).
pub(crate) fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let groups: Vec<&str> = text.split('-').collect();
    let lengths: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    if lengths != [8, 4, 4, 4, 12] {
        return None;
    }
    parse_hex(&groups.concat())?.try_into().ok()
}

/// Append `text` with `\`, `close` and control characters escaped.
pub(crate) fn push_escaped(out: &mut String, text: &str, close: char) {
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == close => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    /// Assert known dates and timestamps render and parse as expected.
    fn known_values() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(parse_date("2000-02-29"), Some(10_957 + 59));
        assert_eq!(parse_date("1900-02-29"), None);
        assert_eq!(parse_date("2026-13-01"), None);
        for days in [i32::MIN, i32::MAX] {
            assert_eq!(parse_date(&format_date(days)), Some(days));
        }
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(-1), "1969-12-31T23:59:59.999999Z");
        assert_eq!(
            parse_timestamp("2026-11-19 10:30:00.5+02:00"),
            parse_timestamp("2026-11-19T08:30:00.500000Z")
        );
        assert_eq!(parse_timestamp("2026-11-19T24:00:00Z"), None);
        assert_eq!(
            format_uuid(&parse_uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11").unwrap()),
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
        );
        assert_eq!(parse_hex("0aFf"), Some(vec![0x0a, 0xff]));
        assert_eq!(parse_hex("0é"), None);
    }

    proptest! {
        #[test]
        /// Assert every date round-trips.
        fn dates_round_trip(days: i32) {
            prop_assert_eq!(parse_date(&format_date(days)), Some(days));
        }

        #[test]
        /// Assert every timestamp round-trips.
        fn timestamps_round_trip(micros: i64) {
            prop_assert_eq!(parse_timestamp(&format_timestamp(micros)), Some(micros));
        }

        #[test]
        /// Assert parsing arbitrary text never panics.
        fn parsing_never_panics(text in "\\PC{0,40}") {
            let _ = parse_date(&text);
            let _ = parse_timestamp(&text);
            let _ = parse_uuid(&text);
            let _ = parse_hex(&text);
        }
    }
}
