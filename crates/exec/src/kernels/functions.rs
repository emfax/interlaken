//! Built-in function kernels.

use std::sync::Arc;

use arrow_array::types::Float64Type;
use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray, PrimitiveArray};
use interlaken_ir::DataType;
use interlaken_ir::interp::EvalError;

use super::compile::{Builtin, Node};
use super::eval::State;
use super::input::Input;
use super::{arith, strings};
use crate::error::ExecError;

/// Call a built-in on every row of `input`.
pub(crate) fn call(
    builtin: Builtin,
    args: &[Node],
    ty: &DataType,
    input: &Input,
    state: &mut State,
) -> Result<ArrayRef, ExecError> {
    let args = args
        .iter()
        .map(|a| a.eval(input, state))
        .collect::<Result<Vec<_>, _>>()?;
    let len = input.len();
    match (builtin, args.as_slice()) {
        (Builtin::Lower, [a]) => Ok(strings::map_text(a, str::to_lowercase)),
        (Builtin::Upper, [a]) => Ok(strings::map_text(a, str::to_uppercase)),
        (Builtin::Length, [a]) => strings::length(a),
        (Builtin::Substr, [t, s, l]) => strings::substr(t, s, l),
        (Builtin::Abs, [a]) => arith::abs(a),
        (Builtin::AssertPresent, [a]) => {
            if a.null_count() > 0 {
                return Err(EvalError::Absent.into());
            }
            Ok(a.clone())
        }
        (Builtin::Random, []) => {
            #[allow(clippy::cast_precision_loss)]
            let result: PrimitiveArray<Float64Type> = (0..len)
                .map(|_| Some((state.next_random() >> 11) as f64 / (1u64 << 53) as f64))
                .collect();
            Ok(Arc::new(result))
        }
        (Builtin::GenRandomUuid, []) => {
            let values = (0..len).map(|_| {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&state.next_random().to_be_bytes());
                bytes[8..].copy_from_slice(&state.next_random().to_be_bytes());
                bytes[6] = (bytes[6] & 0x0f) | 0x40;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                Some(bytes)
            });
            Ok(Arc::new(
                FixedSizeBinaryArray::try_from_sparse_iter_with_size(values, 16)?,
            ))
        }
        (builtin, args) => Err(EvalError::Unsupported(format!(
            "{builtin:?} with {} arguments returning {ty}",
            args.len()
        ))
        .into()),
    }
}
