//! Built-in scalar functions and aggregates.

use std::collections::HashSet;

use super::cast::{int_range_error, int_to, int_value};
use super::datum::{Datum, RowKey};
use super::error::EvalError;
use super::eval::scalar_type_of;
use super::rel::Interp;
use crate::catalog::FunctionDef;
use crate::types::ScalarType;

/// Compute aggregate `def` over the argument tuples of a group's rows.
///
/// `rows` holds one argument tuple per row that passed the `FILTER`.
/// Strict aggregates skip tuples containing NULL; `distinct` keeps one of
/// each distinct tuple. The executor finalizes its aggregates with this,
/// so both compute them identically.
///
/// # Errors
///
/// Returns [`EvalError::OutOfRange`] if an integer sum overflows `int8`, or
/// [`EvalError::Unsupported`] for an aggregate the interpreter lacks.
pub fn aggregate_values(
    def: &FunctionDef,
    rows: Vec<Vec<Datum>>,
    distinct: bool,
) -> Result<Datum, EvalError> {
    Interp::aggregate(def, rows, distinct)
}

/// What `redact` returns.
pub const REDACTED: &str = "****";

/// Replace every character of `text` but the last four with `*` (`mask_last4`).
///
/// # Examples
///
/// ```
/// use interlaken_ir::interp::mask_last4;
///
/// assert_eq!(mask_last4("4111222233334444"), "************4444");
/// assert_eq!(mask_last4("abc"), "abc");
/// ```
#[must_use]
pub fn mask_last4(text: &str) -> String {
    let n = text.chars().count();
    text.chars()
        .enumerate()
        .map(|(i, c)| if i + 4 < n { '*' } else { c })
        .collect()
}

impl Interp<'_> {
    /// Call the scalar function `def` on non-NULL (if strict) arguments.
    pub(crate) fn call(&mut self, def: &FunctionDef, args: Vec<Datum>) -> Result<Datum, EvalError> {
        let name = def.name.to_string();
        Ok(match (name.as_str(), args.as_slice()) {
            ("lower", [Datum::Text(t)]) => Datum::Text(t.to_lowercase()),
            ("upper", [Datum::Text(t)]) => Datum::Text(t.to_uppercase()),
            ("length", [Datum::Text(t)]) => Datum::Int4(
                i32::try_from(t.chars().count()).map_err(|_| int_range_error(ScalarType::Int4))?,
            ),
            ("substr", [Datum::Text(t), Datum::Int4(start), Datum::Int4(len)]) => {
                if *len < 0 {
                    return Err(EvalError::NegativeSubstringLength);
                }
                // Characters at 1-based positions [start, start + len).
                let first = i64::from(*start).max(1);
                let end = i64::from(*start) + i64::from(*len);
                let taken: String = t
                    .chars()
                    .enumerate()
                    .filter(|(i, _)| {
                        let pos = i64::try_from(*i).unwrap_or(i64::MAX) + 1;
                        pos >= first && pos < end
                    })
                    .map(|(_, c)| c)
                    .collect();
                Datum::Text(taken)
            }
            ("abs", [Datum::Float8(v)]) => Datum::Float8(v.abs()),
            ("abs", [d]) if int_value(d).is_some() => int_to(
                int_value(d).expect("integer").abs(),
                scalar_type_of(d).expect("integer"),
            )?,
            ("now", []) => Datum::Timestamptz(self.options.now),
            ("redact", [Datum::Text(_)]) => Datum::Text(REDACTED.into()),
            ("mask_last4", [Datum::Text(t)]) => Datum::Text(mask_last4(t)),
            ("policy_mask", [Datum::Bool(true), value, _]) => value.clone(),
            ("policy_mask", [_, _, masked]) => masked.clone(),
            ("assert_present", [Datum::Null]) => return Err(EvalError::Absent),
            ("assert_present", [v]) => v.clone(),
            ("random", []) => {
                #[allow(clippy::cast_precision_loss)]
                let v = (self.next_random() >> 11) as f64 / (1u64 << 53) as f64;
                Datum::Float8(v)
            }
            ("gen_random_uuid", []) => {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&self.next_random().to_be_bytes());
                bytes[8..].copy_from_slice(&self.next_random().to_be_bytes());
                bytes[6] = (bytes[6] & 0x0f) | 0x40;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                Datum::Uuid(bytes)
            }
            _ => {
                return Err(EvalError::Unsupported(format!(
                    "function {name} on {args:?}"
                )));
            }
        })
    }

    /// Compute aggregate `def` over the argument tuples of the rows it sees.
    ///
    /// `rows` holds one argument tuple per row that passed the `FILTER`.
    /// Strict aggregates skip tuples containing NULL; `distinct` keeps one
    /// of each distinct tuple.
    pub(crate) fn aggregate(
        def: &FunctionDef,
        rows: Vec<Vec<Datum>>,
        distinct: bool,
    ) -> Result<Datum, EvalError> {
        let mut rows: Vec<Vec<Datum>> = if def.properties.strict {
            rows.into_iter()
                .filter(|r| !r.iter().any(Datum::is_null))
                .collect()
        } else {
            rows
        };
        if distinct {
            let mut seen = HashSet::new();
            rows.retain(|r| seen.insert(RowKey(r.clone())));
        }
        let values = || rows.iter().map(|r| &r[0]);
        let name = def.name.to_string();
        Ok(match name.as_str() {
            "count" => Datum::Int8(
                i64::try_from(rows.len()).map_err(|_| int_range_error(ScalarType::Int8))?,
            ),
            "sum" | "avg" if rows.is_empty() => Datum::Null,
            "sum" => match &rows[0][0] {
                Datum::Float8(_) => Datum::Float8(values().map(float).sum()),
                _ => {
                    let total: i128 = values().map(|d| int_value(d).expect("integer")).sum();
                    int_to(total, ScalarType::Int8)?
                }
            },
            "avg" => {
                #[allow(clippy::cast_precision_loss)]
                let n = rows.len() as f64;
                match &rows[0][0] {
                    Datum::Float8(_) => Datum::Float8(values().map(float).sum::<f64>() / n),
                    #[allow(clippy::cast_precision_loss)]
                    _ => Datum::Float8(
                        values()
                            .map(|d| int_value(d).expect("integer"))
                            .sum::<i128>() as f64
                            / n,
                    ),
                }
            }
            "min" | "max" => {
                let want = if name == "min" {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
                values()
                    .fold(None::<&Datum>, |best, d| match best {
                        Some(b) if d.compare(b) != Some(want) => Some(b),
                        _ => Some(d),
                    })
                    .cloned()
                    .unwrap_or(Datum::Null)
            }
            "bool_and" | "bool_or" if rows.is_empty() => Datum::Null,
            "bool_and" => Datum::Bool(values().all(|d| matches!(d, Datum::Bool(true)))),
            "bool_or" => Datum::Bool(values().any(|d| matches!(d, Datum::Bool(true)))),
            _ => return Err(EvalError::Unsupported(format!("aggregate {name}"))),
        })
    }
}

/// Return the value of a float datum.
fn float(d: &Datum) -> f64 {
    match d {
        Datum::Float8(v) => *v,
        _ => unreachable!("float aggregate over a non-float"),
    }
}
