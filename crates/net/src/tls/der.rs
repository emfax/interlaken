//! Reading a certificate's validity period from its DER encoding.
//!
//! webpki checks validity but doesn't expose it; the expiry gauge and the
//! check that a CA is valid now need the dates. A certificate is
//! `SEQUENCE { tbsCertificate, signatureAlgorithm, signature }`, and
//! `tbsCertificate` is `SEQUENCE { [0] version OPTIONAL, serialNumber,
//! signature, issuer, validity, ... }` with `validity` a `SEQUENCE` of two
//! times, each a `UTCTime` or `GeneralizedTime` (RFC 5280).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One DER element: its tag and contents, and what follows it.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// Return the number of days from 1970-01-01 to `year-month-day`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Decode a `UTCTime` (tag 0x17) or `GeneralizedTime` (tag 0x18) in the
/// `Z` form RFC 5280 requires.
fn time(tag: u8, text: &[u8]) -> Option<SystemTime> {
    let text = std::str::from_utf8(text).ok()?;
    let text = text.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if text.len() == 12 => {
            let yy: i64 = text[..2].parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &text[2..])
        }
        0x18 if text.len() == 14 => (text[..4].parse().ok()?, &text[4..]),
        _ => return None,
    };
    let field = |i: usize| -> Option<i64> { rest.get(i..i + 2)?.parse().ok() };
    let (month, day) = (field(0)?, field(2)?);
    let (hour, minute, second) = (field(4)?, field(6)?, field(8)?);
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Return a DER certificate's validity period, `(not_before, not_after)`,
/// or `None` if it doesn't parse.
pub(crate) fn validity(der: &[u8]) -> Option<(SystemTime, SystemTime)> {
    let (_, certificate, _) = element(der)?;
    let (_, mut tbs, _) = element(certificate)?;
    if tbs.first() == Some(&0xa0) {
        tbs = element(tbs)?.2;
    }
    // serialNumber, signature, issuer.
    for _ in 0..3 {
        tbs = element(tbs)?.2;
    }
    let (_, validity, _) = element(tbs)?;
    let (tag, not_before, rest) = element(validity)?;
    let not_before = time(tag, not_before)?;
    let (tag, not_after, _) = element(rest)?;
    Some((not_before, time(tag, not_after)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert civil dates convert to Unix days.
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 11, 25), 20_782);
    }

    #[test]
    /// Assert both time forms decode, and other forms don't.
    fn times() {
        let at = |s: u64| Some(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(time(0x17, b"700101000001Z"), at(1));
        assert_eq!(time(0x18, b"20000301000000Z"), at(11_017 * 86_400));
        assert_eq!(time(0x17, b"700101000001+0100"), None);
        assert_eq!(time(0x05, b"700101000001Z"), None);
    }
}
