//! Evaluating expressions.

use std::cmp::Ordering;

use super::cast::{cast, int_range_error, int_to, int_value};
use super::datum::Datum;
use super::error::EvalError;
use super::rel::Interp;
use crate::expr::{BinaryOp, Expr, UnaryOp};
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionRef};
use crate::types::ScalarType;

/// Column positions: which (binding, column) each value of a row holds.
pub(crate) type Layout = Vec<(BindingId, ColumnId)>;

/// One row visible to expressions, with its layout.
#[derive(Clone, Copy)]
pub(crate) struct Frame<'a> {
    /// What each value is.
    pub(crate) layout: &'a [(BindingId, ColumnId)],
    /// The values.
    pub(crate) row: &'a [Datum],
}

/// Look up a column in the innermost frame that has it.
pub(crate) fn lookup<'a>(
    frames: &[Frame<'a>],
    binding: BindingId,
    column: ColumnId,
) -> Result<&'a Datum, EvalError> {
    for frame in frames {
        if let Some(i) = frame
            .layout
            .iter()
            .position(|&(b, c)| b == binding && c == column)
        {
            return Ok(&frame.row[i]);
        }
    }
    Err(EvalError::Unsupported(format!(
        "column #{}.{} is not in scope",
        binding.get(),
        column.get()
    )))
}

/// Return the scalar type of a non-NULL scalar datum.
pub(crate) fn scalar_type_of(d: &Datum) -> Option<ScalarType> {
    Some(match d {
        Datum::Bool(_) => ScalarType::Bool,
        Datum::Int2(_) => ScalarType::Int2,
        Datum::Int4(_) => ScalarType::Int4,
        Datum::Int8(_) => ScalarType::Int8,
        Datum::Float8(_) => ScalarType::Float8,
        Datum::Text(_) => ScalarType::Text,
        Datum::Bytea(_) => ScalarType::Bytea,
        Datum::Date(_) => ScalarType::Date,
        Datum::Timestamptz(_) => ScalarType::Timestamptz,
        Datum::Uuid(_) => ScalarType::Uuid,
        Datum::Jsonb(_) => ScalarType::Jsonb,
        Datum::Null | Datum::Array(_) | Datum::Record(_) => return None,
    })
}

/// Return the boolean value of a datum: `Some(b)`, or `None` for NULL.
pub(crate) fn truth(d: &Datum) -> Result<Option<bool>, EvalError> {
    match d {
        Datum::Bool(b) => Ok(Some(*b)),
        Datum::Null => Ok(None),
        other => Err(EvalError::Unsupported(format!(
            "{other:?} used as a boolean"
        ))),
    }
}

/// Check a float arithmetic result for overflow and underflow as Postgres does.
fn checked_float(result: f64, a: f64, b: f64, zero_ok: bool) -> Result<Datum, EvalError> {
    if result.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(EvalError::OutOfRange("value out of range: overflow".into()));
    }
    if result == 0.0 && !zero_ok {
        return Err(EvalError::OutOfRange(
            "value out of range: underflow".into(),
        ));
    }
    Ok(Datum::Float8(result))
}

/// Apply integer arithmetic in the type of the operands.
fn int_arith(op: BinaryOp, a: &Datum, b: &Datum) -> Result<Datum, EvalError> {
    let ty = scalar_type_of(a).expect("integer");
    let (x, y) = (
        int_value(a).expect("integer"),
        int_value(b).expect("integer"),
    );
    let result = match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div => {
            if y == 0 {
                return Err(EvalError::DivisionByZero);
            }
            x / y
        }
        BinaryOp::Mod => {
            if y == 0 {
                return Err(EvalError::DivisionByZero);
            }
            x % y
        }
        _ => unreachable!("not arithmetic"),
    };
    int_to(result, ty)
}

/// Apply float arithmetic.
fn float_arith(op: BinaryOp, a: f64, b: f64) -> Result<Datum, EvalError> {
    match op {
        BinaryOp::Add => checked_float(a + b, a, b, true),
        BinaryOp::Sub => checked_float(a - b, a, b, true),
        BinaryOp::Mul => checked_float(a * b, a, b, a == 0.0 || b == 0.0),
        BinaryOp::Div => {
            if b == 0.0 {
                return Err(EvalError::DivisionByZero);
            }
            checked_float(a / b, a, b, a == 0.0 || b.is_infinite())
        }
        _ => unreachable!("not float arithmetic"),
    }
}

/// Return whether `text` matches the `LIKE` pattern, with `\` escaping the next character.
pub(crate) fn like(text: &str, pattern: &str) -> Result<bool, EvalError> {
    /// One element of a compiled pattern.
    enum Token {
        /// `%`: any run of characters.
        Any,
        /// `_`: one character.
        One,
        /// A literal character.
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().ok_or(EvalError::InvalidEscape)?),
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.chars().collect();
    // matches[j]: the first i text characters match the first j tokens.
    let mut matches = vec![false; tokens.len() + 1];
    matches[0] = true;
    for (j, token) in tokens.iter().enumerate() {
        if matches!(token, Token::Any) {
            matches[j + 1] = matches[j];
        }
    }
    for c in &text {
        let mut next = vec![false; tokens.len() + 1];
        for (j, token) in tokens.iter().enumerate() {
            next[j + 1] = match token {
                Token::Any => next[j] || matches[j + 1],
                Token::One => matches[j],
                Token::Char(t) => matches[j] && t == c,
            };
        }
        matches = next;
    }
    Ok(matches[tokens.len()])
}

/// Apply a binary operator to two non-NULL values (or, for `IS [NOT] DISTINCT FROM`, any values).
fn binary(op: BinaryOp, a: &Datum, b: &Datum) -> Result<Datum, EvalError> {
    match op {
        BinaryOp::IsDistinctFrom => return Ok(Datum::Bool(!a.not_distinct(b))),
        BinaryOp::IsNotDistinctFrom => return Ok(Datum::Bool(a.not_distinct(b))),
        _ => {}
    }
    if a.is_null() || b.is_null() {
        return Ok(Datum::Null);
    }
    if op.is_comparison() {
        let ordering = a
            .compare(b)
            .ok_or_else(|| EvalError::Unsupported(format!("comparing {a:?} with {b:?}")))?;
        let result = match op {
            BinaryOp::Eq => ordering == Ordering::Equal,
            BinaryOp::Ne => ordering != Ordering::Equal,
            BinaryOp::Lt => ordering == Ordering::Less,
            BinaryOp::Le => ordering != Ordering::Greater,
            BinaryOp::Gt => ordering == Ordering::Greater,
            _ => ordering != Ordering::Less,
        };
        return Ok(Datum::Bool(result));
    }
    match (op, a, b) {
        (BinaryOp::Add | BinaryOp::Sub, Datum::Date(d), Datum::Int4(n)) => {
            let result = if op == BinaryOp::Add {
                d.checked_add(*n)
            } else {
                d.checked_sub(*n)
            };
            result
                .map(Datum::Date)
                .ok_or_else(|| EvalError::OutOfRange("date".into()))
        }
        (BinaryOp::Sub, Datum::Date(x), Datum::Date(y)) => x
            .checked_sub(*y)
            .map(Datum::Int4)
            .ok_or_else(|| int_range_error(ScalarType::Int4)),
        (
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div,
            Datum::Float8(x),
            Datum::Float8(y),
        ) => float_arith(op, *x, *y),
        (BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod, _, _)
            if int_value(a).is_some() && int_value(b).is_some() =>
        {
            int_arith(op, a, b)
        }
        (BinaryOp::Concat, Datum::Text(x), Datum::Text(y)) => Ok(Datum::Text(format!("{x}{y}"))),
        (BinaryOp::Concat, Datum::Bytea(x), Datum::Bytea(y)) => {
            Ok(Datum::Bytea(x.iter().chain(y).copied().collect()))
        }
        (BinaryOp::Like, Datum::Text(x), Datum::Text(p)) => like(x, p).map(Datum::Bool),
        _ => Err(EvalError::Unsupported(format!("{op:?} on {a:?} and {b:?}"))),
    }
}

impl Interp<'_> {
    /// Evaluate `expr` with `frames` visible, innermost first.
    pub(crate) fn eval(&mut self, expr: &Expr, frames: &[Frame<'_>]) -> Result<Datum, EvalError> {
        Ok(match expr {
            Expr::Column(ColumnRef::Bound { binding, column }) => {
                lookup(frames, *binding, *column)?.clone()
            }
            Expr::Column(ColumnRef::Named { .. }) => {
                return Err(EvalError::Unsupported(
                    "named column in an unresolved plan".into(),
                ));
            }
            Expr::Literal(value) => Datum::from(value),
            Expr::Param { index, .. } => self
                .params
                .get(
                    usize::try_from(*index)
                        .unwrap_or(usize::MAX)
                        .wrapping_sub(1),
                )
                .cloned()
                .ok_or_else(|| EvalError::Unsupported(format!("parameter {index} has no value")))?,
            Expr::Call { func, args } => {
                let FunctionRef::Id(id) = func else {
                    return Err(EvalError::Unsupported("function called by name".into()));
                };
                let def = self.function(*id)?;
                let args = args
                    .iter()
                    .map(|a| self.eval(a, frames))
                    .collect::<Result<Vec<_>, _>>()?;
                if def.properties.strict && args.iter().any(Datum::is_null) {
                    Datum::Null
                } else {
                    self.call(&def, args)?
                }
            }
            Expr::Unary { op, expr } => {
                let v = self.eval(expr, frames)?;
                match op {
                    UnaryOp::IsNull => Datum::Bool(v.is_null()),
                    UnaryOp::IsNotNull => Datum::Bool(!v.is_null()),
                    UnaryOp::Not => truth(&v)?.map_or(Datum::Null, |b| Datum::Bool(!b)),
                    UnaryOp::Neg => match v {
                        Datum::Null => Datum::Null,
                        Datum::Float8(f) => Datum::Float8(-f),
                        other => {
                            let ty = scalar_type_of(&other).expect("numeric");
                            int_to(-int_value(&other).expect("integer"), ty)?
                        }
                    },
                }
            }
            Expr::Binary { op, left, right } => {
                let a = self.eval(left, frames)?;
                let b = self.eval(right, frames)?;
                binary(*op, &a, &b)?
            }
            Expr::And(items) => {
                let mut saw_null = false;
                for item in items {
                    match truth(&self.eval(item, frames)?)? {
                        Some(false) => return Ok(Datum::Bool(false)),
                        None => saw_null = true,
                        Some(true) => {}
                    }
                }
                if saw_null {
                    Datum::Null
                } else {
                    Datum::Bool(true)
                }
            }
            Expr::Or(items) => {
                let mut saw_null = false;
                for item in items {
                    match truth(&self.eval(item, frames)?)? {
                        Some(true) => return Ok(Datum::Bool(true)),
                        None => saw_null = true,
                        Some(false) => {}
                    }
                }
                if saw_null {
                    Datum::Null
                } else {
                    Datum::Bool(false)
                }
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                let operand = operand.as_ref().map(|o| self.eval(o, frames)).transpose()?;
                for branch in branches {
                    let when = self.eval(&branch.when, frames)?;
                    let taken = match &operand {
                        Some(op) => truth(&binary(BinaryOp::Eq, op, &when)?)? == Some(true),
                        None => truth(&when)? == Some(true),
                    };
                    if taken {
                        return self.eval(&branch.then, frames);
                    }
                }
                match otherwise {
                    Some(e) => self.eval(e, frames)?,
                    None => Datum::Null,
                }
            }
            Expr::Coalesce(items) => {
                for item in items {
                    let v = self.eval(item, frames)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Datum::Null
            }
            Expr::NullIf { left, right } => {
                let a = self.eval(left, frames)?;
                let b = self.eval(right, frames)?;
                if truth(&binary(BinaryOp::Eq, &a, &b)?)? == Some(true) {
                    Datum::Null
                } else {
                    a
                }
            }
            Expr::Array(items) => Datum::Array(
                items
                    .iter()
                    .map(|i| self.eval(i, frames))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Cast { expr, to } => cast(self.eval(expr, frames)?, to)?,
            Expr::Subquery { .. } => {
                return Err(EvalError::Unsupported(
                    "subquery in an unresolved plan".into(),
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert LIKE handles wildcards and escapes, and rejects a trailing escape.
    fn like_patterns() {
        assert_eq!(like("abc", "a%"), Ok(true));
        assert_eq!(like("abc", "_b_"), Ok(true));
        assert_eq!(like("abc", "%c%"), Ok(true));
        assert_eq!(like("abc", "b%"), Ok(false));
        assert_eq!(like("a%c", "a\\%c"), Ok(true));
        assert_eq!(like("abc", "a\\%c"), Ok(false));
        assert_eq!(like("", "%"), Ok(true));
        assert_eq!(like("x", "x\\"), Err(EvalError::InvalidEscape));
    }

    #[test]
    /// Assert arithmetic raises Postgres's errors.
    fn arithmetic_errors() {
        assert_eq!(
            binary(BinaryOp::Add, &Datum::Int4(i32::MAX), &Datum::Int4(1)).unwrap_err(),
            EvalError::OutOfRange("integer".into())
        );
        assert_eq!(
            binary(BinaryOp::Div, &Datum::Int8(1), &Datum::Int8(0)).unwrap_err(),
            EvalError::DivisionByZero
        );
        assert!(matches!(
            binary(BinaryOp::Mod, &Datum::Int2(i16::MIN), &Datum::Int2(-1)),
            Ok(Datum::Int2(0))
        ));
        assert!(matches!(
            binary(BinaryOp::Div, &Datum::Int4(-7), &Datum::Int4(2)),
            Ok(Datum::Int4(-3))
        ));
        assert!(binary(BinaryOp::Mul, &Datum::Float8(1e300), &Datum::Float8(1e300)).is_err());
        assert!(matches!(
            binary(
                BinaryOp::Eq,
                &Datum::Float8(f64::NAN),
                &Datum::Float8(f64::NAN)
            ),
            Ok(Datum::Bool(true))
        ));
        assert!(matches!(
            binary(BinaryOp::Eq, &Datum::Null, &Datum::Int4(1)),
            Ok(Datum::Null)
        ));
    }
}
