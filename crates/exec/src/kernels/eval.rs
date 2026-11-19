//! Evaluating compiled expressions over batches.

use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, BooleanArray, UInt32Array};
use interlaken_ir::interp::{Datum, EvalError};
use interlaken_ir::{BinaryOp, UnaryOp};

use super::compile::{Kind, Node};
use super::input::{Input, all_rows, eval_on, merge};
use super::{arith, cast, compare, functions, strings};
use crate::columnar::{build_array, datum_at};
use crate::error::ExecError;

/// Mutable evaluation state.
#[derive(Clone, Debug)]
pub(crate) struct State {
    /// The xorshift64* state for `random()` and `gen_random_uuid()`.
    rng: u64,
}

impl State {
    /// Construct state seeded like the interpreter.
    pub(crate) fn new(seed: u64) -> Self {
        Self { rng: seed | 1 }
    }

    /// Return the next pseudo-random number (xorshift64*).
    pub(crate) fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Return the boolean array of a predicate's result.
pub(crate) fn booleans(array: &ArrayRef) -> Result<&BooleanArray, ExecError> {
    array
        .as_boolean_opt()
        .ok_or_else(|| ExecError::Unsupported(format!("{} used as a boolean", array.data_type())))
}

impl Node {
    /// Evaluate on every row of `input`.
    pub(crate) fn eval(&self, input: &Input, state: &mut State) -> Result<ArrayRef, ExecError> {
        let len = input.len();
        match &self.kind {
            Kind::Column(position) => input.column(*position),
            Kind::Constant(value) => build_array(&self.ty, &vec![value.clone(); len]),
            Kind::Failing(error) if len > 0 => Err(error.clone().into()),
            Kind::Unknown(name) if len > 0 => {
                Err(EvalError::Unsupported(format!("function {name}")).into())
            }
            Kind::Failing(_) | Kind::Unknown(_) => build_array(&self.ty, &[]),
            Kind::Unary(op, child) => {
                let value = child.eval(input, state)?;
                match op {
                    UnaryOp::IsNull => Ok(Arc::new(BooleanArray::from_iter(
                        (0..len).map(|i| Some(value.is_null(i))),
                    ))),
                    UnaryOp::IsNotNull => Ok(Arc::new(BooleanArray::from_iter(
                        (0..len).map(|i| Some(value.is_valid(i))),
                    ))),
                    UnaryOp::Not => Ok(Arc::new(
                        booleans(&value)?
                            .iter()
                            .map(|v| v.map(|b| !b))
                            .collect::<BooleanArray>(),
                    )),
                    UnaryOp::Neg => arith::negate(&value),
                }
            }
            Kind::Binary(op, left, right) => {
                let (l, r) = (left.eval(input, state)?, right.eval(input, state)?);
                match op {
                    BinaryOp::Eq
                    | BinaryOp::Ne
                    | BinaryOp::Lt
                    | BinaryOp::Le
                    | BinaryOp::Gt
                    | BinaryOp::Ge => compare::compare(*op, &l, &r),
                    BinaryOp::IsDistinctFrom => compare::distinct(&l, &r, true),
                    BinaryOp::IsNotDistinctFrom => compare::distinct(&l, &r, false),
                    BinaryOp::Concat => strings::concat(&l, &r),
                    BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod => arith::arithmetic(*op, &l, &r),
                    BinaryOp::Like => Err(ExecError::Unsupported(
                        "LIKE compiled as a plain binary operator".into(),
                    )),
                }
            }
            Kind::Like(text, pattern) => strings::like(text, pattern, input, state),
            Kind::And(items) => self.connective(items, input, state, false),
            Kind::Or(items) => self.connective(items, input, state, true),
            Kind::Case {
                operand,
                branches,
                otherwise,
            } => self.case(
                operand.as_deref(),
                branches,
                otherwise.as_deref(),
                input,
                state,
            ),
            Kind::Coalesce(items) => {
                let mut active = all_rows(len);
                let mut pieces = Vec::new();
                for item in items {
                    if active.is_empty() {
                        break;
                    }
                    let value = eval_on(item, input, &active, state)?;
                    let (mut found, mut rest) = (Vec::new(), Vec::new());
                    let mut positions = Vec::new();
                    for (j, &row) in active.iter().enumerate() {
                        if value.is_valid(j) {
                            found.push(row);
                            positions.push(u32::try_from(j).expect("row positions fit in u32"));
                        } else {
                            rest.push(row);
                        }
                    }
                    if !found.is_empty() {
                        let values = arrow_select::take::take(
                            value.as_ref(),
                            &UInt32Array::from(positions),
                            None,
                        )?;
                        pieces.push((values, found));
                    }
                    active = rest;
                }
                merge(len, &self.ty, &pieces)
            }
            Kind::NullIf(left, right) => {
                let (l, r) = (left.eval(input, state)?, right.eval(input, state)?);
                let equal = compare::compare(BinaryOp::Eq, &l, &r)?;
                Ok(arrow_select::nullif::nullif(l.as_ref(), booleans(&equal)?)?)
            }
            Kind::Array(items) => {
                let values = items
                    .iter()
                    .map(|i| i.eval(input, state))
                    .collect::<Result<Vec<_>, _>>()?;
                let rows = (0..len)
                    .map(|r| {
                        Ok(Datum::Array(
                            values
                                .iter()
                                .map(|v| datum_at(v.as_ref(), r))
                                .collect::<Result<_, ExecError>>()?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ExecError>>()?;
                build_array(&self.ty, &rows)
            }
            Kind::Cast(child) => cast::cast(&child.eval(input, state)?, &child.ty, &self.ty),
            Kind::Try(expr, default) => self.contain(expr, default, input, &all_rows(len), state),
            Kind::Call(builtin, args) => functions::call(*builtin, args, &self.ty, input, state),
        }
    }

    /// Evaluate `AND` (`stop_on` false) or `OR` (`stop_on` true) with three-valued logic.
    ///
    /// A row stops being evaluated once an operand returns `stop_on`; NULLs
    /// are remembered and decide the result if nothing stops the row.
    fn connective(
        &self,
        items: &[Node],
        input: &Input,
        state: &mut State,
        stop_on: bool,
    ) -> Result<ArrayRef, ExecError> {
        let len = input.len();
        let mut stopped = vec![false; len];
        let mut saw_null = vec![false; len];
        let mut active = all_rows(len);
        for item in items {
            if active.is_empty() {
                break;
            }
            let value = eval_on(item, input, &active, state)?;
            let value = booleans(&value)?;
            let mut rest = Vec::with_capacity(active.len());
            for (j, &row) in active.iter().enumerate() {
                match value.is_valid(j).then(|| value.value(j)) {
                    Some(v) if v == stop_on => stopped[row as usize] = true,
                    Some(_) => rest.push(row),
                    None => {
                        saw_null[row as usize] = true;
                        rest.push(row);
                    }
                }
            }
            active = rest;
        }
        let result: BooleanArray = (0..len)
            .map(|r| {
                if stopped[r] {
                    Some(stop_on)
                } else if saw_null[r] {
                    None
                } else {
                    Some(!stop_on)
                }
            })
            .collect();
        Ok(Arc::new(result))
    }

    /// Evaluate `CASE`: each condition only on rows no earlier branch took,
    /// each result only on the rows that take it.
    fn case(
        &self,
        operand: Option<&Node>,
        branches: &[(Node, Node)],
        otherwise: Option<&Node>,
        input: &Input,
        state: &mut State,
    ) -> Result<ArrayRef, ExecError> {
        let len = input.len();
        let operand = operand.map(|o| o.eval(input, state)).transpose()?;
        let mut active = all_rows(len);
        let mut pieces = Vec::new();
        for (when, then) in branches {
            if active.is_empty() {
                break;
            }
            let condition = eval_on(when, input, &active, state)?;
            let condition = match &operand {
                Some(operand) => {
                    let indices = UInt32Array::from(active.clone());
                    let operand = arrow_select::take::take(operand.as_ref(), &indices, None)?;
                    compare::compare(BinaryOp::Eq, &operand, &condition)?
                }
                None => condition,
            };
            let condition = booleans(&condition)?;
            let (mut taken, mut rest) = (Vec::new(), Vec::new());
            for (j, &row) in active.iter().enumerate() {
                if condition.is_valid(j) && condition.value(j) {
                    taken.push(row);
                } else {
                    rest.push(row);
                }
            }
            if !taken.is_empty() {
                pieces.push((eval_on(then, input, &taken, state)?, taken));
            }
            active = rest;
        }
        if let Some(otherwise) = otherwise
            && !active.is_empty()
        {
            pieces.push((eval_on(otherwise, input, &active, state)?, active));
        }
        merge(len, &self.ty, &pieces)
    }

    /// Evaluate `Try(expr, default)` on `rows`: rows where `expr` fails get
    /// `default`, found by bisecting failing sets of rows.
    fn contain(
        &self,
        expr: &Node,
        default: &Node,
        input: &Input,
        rows: &[u32],
        state: &mut State,
    ) -> Result<ArrayRef, ExecError> {
        match eval_on(expr, input, rows, state) {
            Ok(value) => Ok(value),
            // Only evaluation errors are contained; anything else is an engine fault.
            Err(ExecError::Eval(_)) if rows.len() == 1 => eval_on(default, input, rows, state),
            Err(ExecError::Eval(_)) => {
                let (left, right) = rows.split_at(rows.len() / 2);
                let left_values = self.contain(expr, default, input, left, state)?;
                let right_values = self.contain(expr, default, input, right, state)?;
                Ok(arrow_select::concat::concat(&[
                    left_values.as_ref(),
                    right_values.as_ref(),
                ])?)
            }
            Err(other) => Err(other),
        }
    }
}
