//! Text and byte kernels: concatenation, `LIKE`, and the string functions.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Int32Type;
use arrow_array::{Array, ArrayRef, BinaryArray, BooleanArray, Int32Array, StringArray};
use arrow_schema::DataType as ArrowType;
use interlaken_ir::interp::{EvalError, LikePattern};

use super::compile::{Node, Pattern};
use super::eval::State;
use super::input::Input;
use crate::error::ExecError;

/// Concatenate text with text, or bytes with bytes; NULL where either is NULL.
pub(crate) fn concat(a: &ArrayRef, b: &ArrayRef) -> Result<ArrayRef, ExecError> {
    match (a.data_type(), b.data_type()) {
        (ArrowType::Utf8, ArrowType::Utf8) => {
            let result: StringArray = a
                .as_string::<i32>()
                .iter()
                .zip(b.as_string::<i32>().iter())
                .map(|(x, y)| Some(format!("{}{}", x?, y?)))
                .collect();
            Ok(Arc::new(result))
        }
        (ArrowType::Binary, ArrowType::Binary) => {
            let result: BinaryArray = a
                .as_binary::<i32>()
                .iter()
                .zip(b.as_binary::<i32>().iter())
                .map(|(x, y)| Some([x?, y?].concat()))
                .collect();
            Ok(Arc::new(result))
        }
        (x, y) => Err(EvalError::Unsupported(format!("concatenating {x} and {y}")).into()),
    }
}

/// Evaluate `text LIKE pattern`; NULL where either is NULL.
///
/// A malformed pattern raises only for rows whose text is not NULL, as in
/// the interpreter, which checks for NULL before matching.
pub(crate) fn like(
    text: &Node,
    pattern: &Pattern,
    input: &Input,
    state: &mut State,
) -> Result<ArrayRef, ExecError> {
    let texts = text.eval(input, state)?;
    let texts = texts.as_string::<i32>();
    let result: BooleanArray = match pattern {
        Pattern::Null => (0..texts.len()).map(|_| None::<bool>).collect(),
        Pattern::Constant(compiled) => {
            if texts.null_count() == texts.len() {
                (0..texts.len()).map(|_| None::<bool>).collect()
            } else {
                let compiled = compiled.as_ref().map_err(|e| ExecError::Eval(e.clone()))?;
                texts
                    .iter()
                    .map(|t| t.map(|t| compiled.matches(t)))
                    .collect()
            }
        }
        Pattern::Dynamic(node) => {
            let patterns = node.eval(input, state)?;
            let patterns = patterns.as_string::<i32>();
            // Patterns usually repeat, so keep the last one compiled.
            let mut last: Option<(String, LikePattern)> = None;
            texts
                .iter()
                .zip(patterns.iter())
                .map(|(t, p)| {
                    let (Some(t), Some(p)) = (t, p) else {
                        return Ok(None);
                    };
                    if last.as_ref().is_none_or(|(text, _)| text != p) {
                        last = Some((p.to_owned(), LikePattern::compile(p)?));
                    }
                    Ok(Some(last.as_ref().expect("compiled above").1.matches(t)))
                })
                .collect::<Result<_, EvalError>>()?
        }
    };
    Ok(Arc::new(result))
}

/// Map each non-NULL string through `f`.
pub(crate) fn map_text(a: &ArrayRef, f: impl Fn(&str) -> String) -> ArrayRef {
    let result: StringArray = a.as_string::<i32>().iter().map(|t| t.map(&f)).collect();
    Arc::new(result)
}

/// Return the length of each string in characters.
pub(crate) fn length(a: &ArrayRef) -> Result<ArrayRef, ExecError> {
    let result: Int32Array = a
        .as_string::<i32>()
        .iter()
        .map(|t| {
            t.map(|t| {
                i32::try_from(t.chars().count())
                    .map_err(|_| EvalError::OutOfRange("integer".into()))
            })
            .transpose()
        })
        .collect::<Result<_, _>>()?;
    Ok(Arc::new(result))
}

/// Return the characters at 1-based positions `[start, start + len)` of each string.
pub(crate) fn substr(
    text: &ArrayRef,
    start: &ArrayRef,
    len: &ArrayRef,
) -> Result<ArrayRef, ExecError> {
    let (texts, starts, lens) = (
        text.as_string::<i32>(),
        start.as_primitive::<Int32Type>(),
        len.as_primitive::<Int32Type>(),
    );
    let result: StringArray = (0..texts.len())
        .map(|i| {
            if texts.is_null(i) || starts.is_null(i) || lens.is_null(i) {
                return Ok(None);
            }
            let (start, len) = (starts.value(i), lens.value(i));
            if len < 0 {
                return Err(EvalError::NegativeSubstringLength);
            }
            let first = i64::from(start).max(1);
            let end = i64::from(start) + i64::from(len);
            let taken: String = texts
                .value(i)
                .chars()
                .enumerate()
                .filter(|(p, _)| {
                    let pos = i64::try_from(*p).unwrap_or(i64::MAX) + 1;
                    pos >= first && pos < end
                })
                .map(|(_, c)| c)
                .collect();
            Ok(Some(taken))
        })
        .collect::<Result<_, EvalError>>()?;
    Ok(Arc::new(result))
}
