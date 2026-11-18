//! Casts between types, with Postgres's rules and text formats.

use super::datum::Datum;
use super::error::EvalError;
use crate::text::literal::{format_hex, format_uuid, parse_date, parse_timestamp, parse_uuid};
use crate::types::{DataType, ScalarType};

/// Microseconds in a day.
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Construct an invalid-text error for `ty`.
fn invalid(ty: ScalarType, text: &str) -> EvalError {
    EvalError::InvalidText {
        ty: ty.name().into(),
        text: text.into(),
    }
}

/// Return the out-of-range error Postgres raises for an integer type.
pub(crate) fn int_range_error(ty: ScalarType) -> EvalError {
    EvalError::OutOfRange(
        match ty {
            ScalarType::Int2 => "smallint",
            ScalarType::Int4 => "integer",
            _ => "bigint",
        }
        .into(),
    )
}

/// Convert an integer to the integer type `ty`, failing if it does not fit.
pub(crate) fn int_to(v: i128, ty: ScalarType) -> Result<Datum, EvalError> {
    let out_of_range = || int_range_error(ty);
    Ok(match ty {
        ScalarType::Int2 => Datum::Int2(i16::try_from(v).map_err(|_| out_of_range())?),
        ScalarType::Int4 => Datum::Int4(i32::try_from(v).map_err(|_| out_of_range())?),
        ScalarType::Int8 => Datum::Int8(i64::try_from(v).map_err(|_| out_of_range())?),
        _ => unreachable!("int_to is only called with integer types"),
    })
}

/// Return the value of an integer datum.
pub(crate) fn int_value(d: &Datum) -> Option<i128> {
    match d {
        Datum::Int2(v) => Some(i128::from(*v)),
        Datum::Int4(v) => Some(i128::from(*v)),
        Datum::Int8(v) => Some(i128::from(*v)),
        _ => None,
    }
}

/// Render a float as Postgres does: the shortest text that reads back
/// exactly, in plain notation for decimal exponents from -4 to 14 and
/// scientific notation (`1.5e+20`, `1e-07`) otherwise.
pub(crate) fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // `{:e}` gives the shortest round-trip digits, e.g. "1.5e20" or "-1e-7".
    let sci = format!("{v:e}");
    let (mantissa, exponent) = sci.split_once('e').expect("{:e} always has an exponent");
    let exponent: i32 = exponent.parse().expect("integer exponent");
    if (-4..15).contains(&exponent) {
        let (negative, mantissa) = match mantissa.strip_prefix('-') {
            Some(m) => (true, m),
            None => (false, mantissa),
        };
        let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
        let point = exponent + 1;
        let mut out = String::from(if negative { "-" } else { "" });
        if point <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n(
                '0',
                usize::try_from(-point).unwrap_or(0),
            ));
            out.push_str(&digits);
        } else {
            let point = usize::try_from(point).expect("positive");
            if digits.len() <= point {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
        out
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    }
}

/// Render a date as Postgres does in ISO style (`YYYY-MM-DD`, with ` BC` for years before 1).
fn format_date(days: i32) -> String {
    let iso = crate::text::literal::format_date(days);
    match iso.strip_prefix('-') {
        // ISO year -44 is 45 BC: there is no year 0.
        Some(rest) => {
            let (year, tail) = rest.split_once('-').expect("date has a year");
            let year: i64 = year.parse().expect("numeric year");
            format!("{:04}-{tail} BC", year + 1)
        }
        None if iso.starts_with("0000") => format!("0001{} BC", &iso[4..]),
        None => iso,
    }
}

/// Render a timestamp as Postgres does with the time zone set to UTC (`2026-11-19 10:30:00.5+00`).
fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let date = format_date(i32::try_from(days).unwrap_or(i32::MAX));
    let (date, era) = match date.strip_suffix(" BC") {
        Some(d) => (d.to_owned(), " BC"),
        None => (date, ""),
    };
    let secs = of_day / 1_000_000;
    let mut out = format!(
        "{date} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    let fraction = of_day % 1_000_000;
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out.push_str("+00");
    out.push_str(era);
    out
}

/// Render a value as text, as a cast to `text` does.
pub(crate) fn to_text(d: &Datum) -> Result<String, EvalError> {
    Ok(match d {
        Datum::Bool(v) => if *v { "true" } else { "false" }.into(),
        Datum::Int2(v) => v.to_string(),
        Datum::Int4(v) => v.to_string(),
        Datum::Int8(v) => v.to_string(),
        Datum::Float8(v) => format_float(*v),
        Datum::Text(v) | Datum::Jsonb(v) => v.clone(),
        Datum::Bytea(v) => format!("\\x{}", format_hex(v)),
        Datum::Date(v) => format_date(*v),
        Datum::Timestamptz(v) => format_timestamp(*v),
        Datum::Uuid(v) => format_uuid(v),
        Datum::Null | Datum::Array(_) | Datum::Record(_) => {
            return Err(EvalError::Unsupported(
                "text form of arrays and records".into(),
            ));
        }
    })
}

/// Parse text as a value of `ty`, as a cast from `text` does.
fn from_text(text: &str, ty: ScalarType) -> Result<Datum, EvalError> {
    let trimmed = text.trim();
    let bad = || invalid(ty, text);
    Ok(match ty {
        ScalarType::Text => Datum::Text(text.into()),
        ScalarType::Jsonb => Datum::Jsonb(text.into()),
        ScalarType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "t" | "true" | "y" | "yes" | "on" | "1" => Datum::Bool(true),
            "f" | "false" | "n" | "no" | "off" | "0" => Datum::Bool(false),
            _ => return Err(bad()),
        },
        ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8 => {
            let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
            if digits.is_empty()
                || !digits
                    .trim_start_matches('-')
                    .bytes()
                    .all(|b| b.is_ascii_digit())
            {
                return Err(bad());
            }
            let v: i128 = digits.parse().map_err(|_| int_range_error(ty))?;
            int_to(v, ty)?
        }
        ScalarType::Float8 => {
            let lower = trimmed.to_ascii_lowercase();
            let v = match lower.as_str() {
                "nan" => f64::NAN,
                "infinity" | "inf" | "+infinity" | "+inf" => f64::INFINITY,
                "-infinity" | "-inf" => f64::NEG_INFINITY,
                _ => {
                    let v: f64 = lower.parse().map_err(|_| bad())?;
                    if v.is_infinite() {
                        return Err(EvalError::OutOfRange("double precision".into()));
                    }
                    v
                }
            };
            Datum::Float8(v)
        }
        ScalarType::Bytea => {
            let hex = trimmed.strip_prefix("\\x").ok_or_else(bad)?;
            Datum::Bytea(crate::text::literal::parse_hex(hex).ok_or_else(bad)?)
        }
        ScalarType::Date => Datum::Date(parse_date(trimmed).ok_or_else(bad)?),
        ScalarType::Timestamptz => Datum::Timestamptz(parse_timestamp(trimmed).ok_or_else(bad)?),
        ScalarType::Uuid => Datum::Uuid(parse_uuid(trimmed).ok_or_else(bad)?),
    })
}

/// Cast a value to `to`. NULL casts to NULL.
pub(crate) fn cast(d: Datum, to: &DataType) -> Result<Datum, EvalError> {
    if d.is_null() {
        return Ok(Datum::Null);
    }
    let DataType::Scalar(target) = to else {
        // Only identity casts exist between non-scalar types.
        return Ok(d);
    };
    use ScalarType as S;
    Ok(match (d, *target) {
        (d, S::Text) => Datum::Text(to_text(&d)?),
        (Datum::Text(t), ty) => from_text(&t, ty)?,
        (
            d @ (Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)),
            ty @ (S::Int2 | S::Int4 | S::Int8),
        ) => int_to(int_value(&d).expect("integer"), ty)?,
        #[allow(clippy::cast_precision_loss)]
        (d @ (Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)), S::Float8) => {
            Datum::Float8(int_value(&d).expect("integer") as f64)
        }
        (Datum::Float8(v), ty @ (S::Int2 | S::Int4 | S::Int8)) => {
            // Postgres rounds half to even (rint).
            let rounded = v.round_ties_even();
            if !rounded.is_finite() || rounded.abs() >= 2f64.powi(63) {
                return Err(int_range_error(ty));
            }
            #[allow(clippy::cast_possible_truncation)]
            int_to(i128::from(rounded as i64), ty)?
        }
        (d @ (Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_)), S::Bool) => {
            Datum::Bool(int_value(&d).expect("integer") != 0)
        }
        (Datum::Bool(b), ty @ (S::Int2 | S::Int4 | S::Int8)) => int_to(i128::from(b), ty)?,
        (Datum::Date(days), S::Timestamptz) => Datum::Timestamptz(i64::from(days) * MICROS_PER_DAY),
        (Datum::Timestamptz(micros), S::Date) => Datum::Date(
            i32::try_from(micros.div_euclid(MICROS_PER_DAY))
                .map_err(|_| EvalError::OutOfRange("date".into()))?,
        ),
        (d, ty) if super::eval::scalar_type_of(&d) == Some(ty) => d,
        (d, ty) => {
            return Err(EvalError::Unsupported(format!(
                "cast from {d:?} to {}",
                ty.name()
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert floats render like Postgres's float8out.
    fn float_text() {
        for (v, text) in [
            (1.5, "1.5"),
            (0.1, "0.1"),
            (100.0, "100"),
            (1e14, "100000000000000"),
            (1e15, "1e+15"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (123_456.789, "123456.789"),
            (f64::NAN, "NaN"),
            (-0.0, "-0"),
        ] {
            assert_eq!(format_float(v), text, "{v}");
        }
    }

    #[test]
    /// Assert dates and timestamps render like Postgres in UTC.
    fn date_text() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(-719_528), "0001-01-01 BC");
        assert_eq!(format_timestamp(-1), "1969-12-31 23:59:59.999999+00");
        assert_eq!(format_timestamp(500_000), "1970-01-01 00:00:00.5+00");
    }

    #[test]
    /// Assert casts round, range-check and parse as Postgres does.
    fn casts() {
        let int4 = DataType::Scalar(ScalarType::Int4);
        assert!(matches!(
            cast(Datum::Float8(2.5), &int4),
            Ok(Datum::Int4(2))
        ));
        assert!(matches!(
            cast(Datum::Float8(3.5), &int4),
            Ok(Datum::Int4(4))
        ));
        assert!(matches!(
            cast(Datum::Int8(1 << 40), &int4),
            Err(EvalError::OutOfRange(_))
        ));
        assert!(matches!(
            cast(Datum::Text(" 42 ".into()), &int4),
            Ok(Datum::Int4(42))
        ));
        assert!(matches!(
            cast(Datum::Text("4x".into()), &int4),
            Err(EvalError::InvalidText { .. })
        ));
        let bool_type = DataType::Scalar(ScalarType::Bool);
        assert!(matches!(
            cast(Datum::Text("Yes".into()), &bool_type),
            Ok(Datum::Bool(true))
        ));
        assert!(matches!(cast(Datum::Null, &bool_type), Ok(Datum::Null)));
    }
}
