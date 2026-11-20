//! Incremental aggregate accumulators (milestone F3, backlog `incremental-aggregates`).
//!
//! `count`, `sum`, `avg`, `min`, `max`, `bool_and` and `bool_or` (without
//! `DISTINCT`) fold each argument into a fixed-size state as rows arrive, so
//! an aggregation holds memory proportional to its groups, not its rows.
//! Every fold is exactly what the interpreter's `aggregate_values` computes
//! over the kept argument tuples:
//!
//! - integer `sum` and `avg` add in `i128`; a sum outside `int8` fails when
//!   the group finishes, as the interpreter's does;
//! - float `sum` and `avg` add left to right from the empty sum, which is how
//!   Rust's `Iterator::sum` (the interpreter's) folds, so the result is
//!   bit-identical in row order;
//! - `min` and `max` keep the first of equal values, with the interpreter's
//!   value comparison;
//! - all of them are strict: rows whose argument is NULL are skipped, and a
//!   group that saw no other row finishes as NULL (`count` as 0).
//!
//! Any other aggregate, any `DISTINCT` aggregate and any with several
//! arguments keeps its argument tuples and is finished by the interpreter.

use std::cmp::Ordering;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int16Type, Int32Type, Int64Type};
use arrow_array::{Array, BooleanArray, Float64Array, Int16Array, Int32Array, Int64Array};
use arrow_array::{ArrayRef, StringArray};
use arrow_schema::DataType as ArrowType;
use interlaken_ir::interp::{Datum, EvalError, aggregate_values};
use interlaken_ir::{DataType, ScalarType};

use crate::columnar::datum_at;
use crate::error::ExecError;
use crate::memory::{datum_bytes, row_bytes};
use crate::physical::Aggregate;

/// How one aggregate accumulates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A non-distinct `count`, of rows or of rows whose argument is not NULL.
    Count,
    /// `sum` over integers.
    IntSum,
    /// `sum` over floats.
    FloatSum,
    /// `avg` over integers.
    IntAvg,
    /// `avg` over floats.
    FloatAvg,
    /// `min` or `max`: the value that compares as `want` against every other.
    Best(Ordering),
    /// `bool_and` (`true`) or `bool_or` (`false`).
    Bool(bool),
    /// Anything else: keep the argument tuples for the interpreter.
    Tuples,
}

impl Kind {
    /// Choose how `agg` accumulates.
    pub(crate) fn of(agg: &Aggregate) -> Self {
        let def = &agg.def;
        let name = def.name.to_string();
        if name == "count" && !agg.distinct {
            return Self::Count;
        }
        if agg.distinct || agg.args.len() != 1 || !def.properties.strict || def.args.len() != 1 {
            return Self::Tuples;
        }
        let int = matches!(
            def.args[0],
            DataType::Scalar(ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8)
        );
        let float = def.args[0] == DataType::Scalar(ScalarType::Float8);
        match name.as_str() {
            "sum" if int => Self::IntSum,
            "sum" if float => Self::FloatSum,
            "avg" if int => Self::IntAvg,
            "avg" if float => Self::FloatAvg,
            "min" => Self::Best(Ordering::Less),
            "max" => Self::Best(Ordering::Greater),
            "bool_and" => Self::Bool(true),
            "bool_or" => Self::Bool(false),
            _ => Self::Tuples,
        }
    }

    /// Return the state of a group that has seen no rows.
    pub(crate) fn fresh(self) -> Accumulator {
        match self {
            Self::Count => Accumulator::Count(0),
            Self::IntSum | Self::IntAvg => Accumulator::Int { total: 0, rows: 0 },
            Self::FloatSum | Self::FloatAvg => Accumulator::Float {
                total: empty_sum(),
                rows: 0,
            },
            Self::Best(_) => Accumulator::Best(None),
            Self::Bool(_) => Accumulator::Bool(None),
            Self::Tuples => Accumulator::Tuples(Vec::new()),
        }
    }
}

/// Return the sum of no floats as the interpreter computes it.
fn empty_sum() -> f64 {
    std::iter::empty::<f64>().sum()
}

/// One aggregate's state for one group.
#[derive(Clone, Debug)]
pub(crate) enum Accumulator {
    /// The rows counted.
    Count(i64),
    /// An integer total and the rows added.
    Int {
        /// The total.
        total: i128,
        /// The rows added.
        rows: i64,
    },
    /// A float total, added left to right, and the rows added.
    Float {
        /// The total.
        total: f64,
        /// The rows added.
        rows: i64,
    },
    /// The best value so far.
    Best(Option<Datum>),
    /// The conjunction or disjunction so far.
    Bool(Option<bool>),
    /// The argument tuples of the rows seen.
    Tuples(Vec<Vec<Datum>>),
}

/// One argument column of a batch, downcast once.
pub(crate) enum Arg<'a> {
    /// No argument (`count(*)`).
    None,
    /// `int2` values.
    Int16(&'a Int16Array),
    /// `int4` values.
    Int32(&'a Int32Array),
    /// `int8` values.
    Int64(&'a Int64Array),
    /// `float8` values.
    Float64(&'a Float64Array),
    /// `bool` values.
    Bool(&'a BooleanArray),
    /// Text values.
    Text(&'a StringArray),
    /// Values of any other type, read as values.
    Other(&'a dyn Array),
}

impl<'a> Arg<'a> {
    /// View the first of an aggregate's evaluated arguments.
    pub(crate) fn of(args: &'a [ArrayRef]) -> Self {
        let Some(array) = args.first() else {
            return Self::None;
        };
        let array = array.as_ref();
        match array.data_type() {
            ArrowType::Int16 => Self::Int16(array.as_primitive::<Int16Type>()),
            ArrowType::Int32 => Self::Int32(array.as_primitive::<Int32Type>()),
            ArrowType::Int64 => Self::Int64(array.as_primitive::<Int64Type>()),
            ArrowType::Float64 => Self::Float64(array.as_primitive::<Float64Type>()),
            ArrowType::Boolean => Self::Bool(array.as_boolean()),
            ArrowType::Utf8 => Self::Text(array.as_string::<i32>()),
            _ => Self::Other(array),
        }
    }

    /// Return whether row `k` is NULL.
    fn is_null(&self, k: usize) -> bool {
        match self {
            Self::None => false,
            Self::Int16(a) => a.is_null(k),
            Self::Int32(a) => a.is_null(k),
            Self::Int64(a) => a.is_null(k),
            Self::Float64(a) => a.is_null(k),
            Self::Bool(a) => a.is_null(k),
            Self::Text(a) => a.is_null(k),
            Self::Other(a) => a.is_null(k),
        }
    }

    /// Return row `k` (not NULL) as an integer.
    fn int(&self, k: usize) -> Result<i128, ExecError> {
        Ok(match self {
            Self::Int16(a) => i128::from(a.value(k)),
            Self::Int32(a) => i128::from(a.value(k)),
            Self::Int64(a) => i128::from(a.value(k)),
            _ => return Err(ExecError::Unsupported("integer aggregate argument".into())),
        })
    }

    /// Return row `k` (not NULL) as a float.
    fn float(&self, k: usize) -> Result<f64, ExecError> {
        match self {
            Self::Float64(a) => Ok(a.value(k)),
            _ => Err(ExecError::Unsupported("float aggregate argument".into())),
        }
    }

    /// Return row `k` (not NULL) as a boolean.
    fn boolean(&self, k: usize) -> Result<bool, ExecError> {
        match self {
            Self::Bool(a) => Ok(a.value(k)),
            _ => Err(ExecError::Unsupported("boolean aggregate argument".into())),
        }
    }

    /// Return row `k` as a value.
    fn datum(&self, k: usize) -> Result<Datum, ExecError> {
        match self {
            Self::None => Ok(Datum::Null),
            Self::Int16(a) => datum_at(*a, k),
            Self::Int32(a) => datum_at(*a, k),
            Self::Int64(a) => datum_at(*a, k),
            Self::Float64(a) => datum_at(*a, k),
            Self::Bool(a) => datum_at(*a, k),
            Self::Text(a) => datum_at(*a, k),
            Self::Other(a) => datum_at(*a, k),
        }
    }
}

impl Accumulator {
    /// Fold row `k` of `arg` into an incremental state; return the change in its heap bytes.
    ///
    /// `strict` says whether rows whose argument is NULL are skipped.
    /// Tuple-keeping states take whole tuples ([`Accumulator::push_tuple`]).
    pub(crate) fn update(
        &mut self,
        kind: Kind,
        arg: &Arg<'_>,
        k: usize,
        strict: bool,
    ) -> Result<isize, ExecError> {
        if strict && arg.is_null(k) {
            return Ok(0);
        }
        match (self, kind) {
            (Self::Count(n), _) => *n += 1,
            (Self::Int { total, rows }, _) => {
                *total += arg.int(k)?;
                *rows += 1;
            }
            (Self::Float { total, rows }, _) => {
                *total += arg.float(k)?;
                *rows += 1;
            }
            (Self::Best(best), Kind::Best(want)) => return Self::fold_best(best, want, arg, k),
            (Self::Bool(acc), Kind::Bool(and)) => {
                let v = arg.boolean(k)?;
                let folded = match *acc {
                    None => v,
                    Some(a) if and => a && v,
                    Some(a) => a || v,
                };
                *acc = Some(folded);
            }
            (state, kind) => {
                return Err(ExecError::Unsupported(format!(
                    "aggregate state {state:?} for {kind:?}"
                )));
            }
        }
        Ok(0)
    }

    /// Keep row `k` of `arg` in `best` if it compares as `want` against the value held.
    fn fold_best(
        best: &mut Option<Datum>,
        want: Ordering,
        arg: &Arg<'_>,
        k: usize,
    ) -> Result<isize, ExecError> {
        // Text is compared in place, so a value that loses is never copied.
        if let (Arg::Text(a), Some(Datum::Text(held))) = (arg, &*best)
            && a.value(k).as_bytes().cmp(held.as_bytes()) != want
        {
            return Ok(0);
        }
        let value = arg.datum(k)?;
        let replace = match &*best {
            None => true,
            Some(held) => value.compare(held) == Some(want),
        };
        if !replace {
            return Ok(0);
        }
        let old = best.as_ref().map_or(0, datum_bytes);
        let new = datum_bytes(&value);
        *best = Some(value);
        Ok(isize::try_from(new).unwrap_or(isize::MAX) - isize::try_from(old).unwrap_or(0))
    }

    /// Add one argument tuple to a tuple-keeping state; return its bytes.
    pub(crate) fn push_tuple(&mut self, tuple: Vec<Datum>) -> Result<usize, ExecError> {
        match self {
            Self::Tuples(tuples) => {
                let bytes = row_bytes(&tuple);
                tuples.push(tuple);
                Ok(bytes)
            }
            other => Err(ExecError::Unsupported(format!(
                "argument tuple for aggregate state {other:?}"
            ))),
        }
    }

    /// Compute the aggregate's value for the group.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::OutOfRange`] if an integer sum does not fit
    /// `int8`, or the interpreter's error for kept tuples.
    pub(crate) fn finish(self, agg: &Aggregate, kind: Kind) -> Result<Datum, ExecError> {
        Ok(match (self, kind) {
            (Self::Count(n), _) => Datum::Int8(n),
            (Self::Int { rows: 0, .. } | Self::Float { rows: 0, .. }, _)
            | (Self::Best(None), _)
            | (Self::Bool(None), _) => Datum::Null,
            (Self::Int { total, .. }, Kind::IntSum) => Datum::Int8(
                i64::try_from(total).map_err(|_| EvalError::OutOfRange("bigint".into()))?,
            ),
            (Self::Int { total, rows }, _) => Datum::Float8(total as f64 / rows as f64),
            (Self::Float { total, .. }, Kind::FloatSum) => Datum::Float8(total),
            (Self::Float { total, rows }, _) => Datum::Float8(total / rows as f64),
            (Self::Best(Some(value)), _) => value,
            (Self::Bool(Some(value)), _) => Datum::Bool(value),
            (Self::Tuples(tuples), _) => aggregate_values(&agg.def, tuples, agg.distinct)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};
    use interlaken_ir::catalog::Catalog;

    use super::*;

    /// Return the built-in aggregate `name` over `arg`, without `DISTINCT` or `FILTER`.
    fn aggregate(name: &str, arg: ScalarType) -> Aggregate {
        let catalog = interlaken_ir::text::parse_catalog("(catalog (builtins))").unwrap();
        let def = catalog
            .function_by_name(&name.into(), &[DataType::Scalar(arg)])
            .unwrap();
        Aggregate {
            def,
            args: vec![interlaken_ir::Expr::Literal(interlaken_ir::Value::Null)],
            distinct: false,
            filter: None,
        }
    }

    /// Fold every row of `array` and compare with the interpreter over the same values.
    fn agrees(name: &str, ty: ScalarType, array: ArrayRef) {
        let agg = aggregate(name, ty);
        let kind = Kind::of(&agg);
        assert_ne!(kind, Kind::Tuples, "{name} should accumulate incrementally");
        let mut state = kind.fresh();
        let args = [array.clone()];
        let arg = Arg::of(&args);
        for k in 0..array.len() {
            state.update(kind, &arg, k, true).unwrap();
        }
        let tuples: Vec<Vec<Datum>> = (0..array.len())
            .map(|k| vec![datum_at(array.as_ref(), k).unwrap()])
            .collect();
        let expected = aggregate_values(&agg.def, tuples, false);
        let actual = state.finish(&agg, kind);
        match (expected, actual) {
            (Ok(e), Ok(a)) => {
                assert_eq!(format!("{e:?}"), format!("{a:?}"), "{name} over {array:?}")
            }
            (Err(e), Err(a)) => assert_eq!(ExecError::from(e).to_string(), a.to_string()),
            (e, a) => panic!("{name} over {array:?}: expected {e:?}, got {a:?}"),
        }
    }

    #[test]
    /// Assert float sums and averages are bit-identical to the interpreter's,
    /// including cancellation, negative zero, infinities and NaN.
    fn float_folds_match_the_interpreter() {
        let cases: Vec<Vec<Option<f64>>> = vec![
            vec![],
            vec![None],
            vec![Some(-0.0)],
            vec![Some(-0.0), Some(-0.0)],
            vec![Some(-0.0), Some(0.0)],
            vec![Some(1e16), Some(1.0), Some(-1e16), Some(1.0)],
            vec![Some(0.1), Some(0.2), Some(0.3), None, Some(-0.6)],
            vec![Some(f64::INFINITY), Some(f64::NEG_INFINITY)],
            vec![Some(f64::NAN), Some(1.0)],
            vec![Some(f64::MAX), Some(f64::MAX), Some(-f64::MAX)],
        ];
        for values in cases {
            for name in ["sum", "avg", "min", "max"] {
                agrees(
                    name,
                    ScalarType::Float8,
                    Arc::new(Float64Array::from(values.clone())),
                );
            }
        }
    }

    #[test]
    /// Assert integer sums fail past `int8` as the interpreter's do, and
    /// that text extremes keep the first of equal values.
    fn integer_and_text_folds_match_the_interpreter() {
        for values in [
            vec![Some(i64::MAX), Some(1), Some(-2)],
            vec![Some(i64::MAX), Some(1)],
            vec![Some(i64::MIN), Some(-1)],
            vec![None, None],
            vec![Some(3), None, Some(-3)],
        ] {
            for name in ["sum", "avg", "min", "max"] {
                agrees(
                    name,
                    ScalarType::Int8,
                    Arc::new(Int64Array::from(values.clone())),
                );
            }
        }
        for values in [
            vec![Some("b"), Some("a"), Some("ab"), None, Some("")],
            vec![None],
        ] {
            for name in ["min", "max"] {
                agrees(
                    name,
                    ScalarType::Text,
                    Arc::new(StringArray::from(values.clone())),
                );
            }
        }
    }
}
