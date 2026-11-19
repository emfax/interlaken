//! Kernel inputs, and restricting evaluation to a subset of rows.

use arrow_array::{Array, ArrayRef, UInt32Array, new_empty_array, new_null_array};
use interlaken_ir::DataType;

use super::compile::Node;
use super::eval::State;
use crate::columnar::arrow_type;
use crate::error::ExecError;

/// The columns an expression reads, all with `len` rows.
///
/// Columns a restricted subtree does not read are left out (`None`) rather
/// than copied.
pub(crate) struct Input {
    /// The columns, by input position.
    columns: Vec<Option<ArrayRef>>,
    /// The number of rows.
    len: usize,
}

impl Input {
    /// Construct an input of `len` rows.
    pub(crate) fn new(columns: Vec<Option<ArrayRef>>, len: usize) -> Self {
        Self { columns, len }
    }

    /// Return the number of rows.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Return the column at `position`.
    pub(crate) fn column(&self, position: usize) -> Result<ArrayRef, ExecError> {
        self.columns
            .get(position)
            .cloned()
            .flatten()
            .ok_or_else(|| {
                ExecError::Unsupported(format!("input column {position} was not materialized"))
            })
    }

    /// Return a new input holding only `rows` (ascending), with only the columns `cols`.
    fn restrict(&self, rows: &[u32], cols: &[usize]) -> Result<Self, ExecError> {
        let indices = UInt32Array::from(rows.to_vec());
        let mut columns = vec![None; self.columns.len()];
        for &c in cols {
            columns[c] = Some(arrow_select::take::take(
                self.column(c)?.as_ref(),
                &indices,
                None,
            )?);
        }
        Ok(Self {
            columns,
            len: rows.len(),
        })
    }
}

/// Return the row numbers `0..len`.
pub(crate) fn all_rows(len: usize) -> Vec<u32> {
    (0..u32::try_from(len).expect("batches have fewer than 2^32 rows")).collect()
}

/// Evaluate `node` on `rows` of `input` only (ascending), returning one value per selected row.
///
/// No rows means no evaluation at all, so nothing can raise an error.
pub(crate) fn eval_on(
    node: &Node,
    input: &Input,
    rows: &[u32],
    state: &mut State,
) -> Result<ArrayRef, ExecError> {
    if rows.is_empty() {
        return Ok(new_empty_array(&arrow_type(&node.ty)));
    }
    if rows.len() == input.len() {
        return node.eval(input, state);
    }
    node.eval(&input.restrict(rows, &node.cols)?, state)
}

/// Assemble a column of `len` rows of type `ty` from pieces, each holding
/// the values for its rows (ascending); rows in no piece are NULL.
pub(crate) fn merge(
    len: usize,
    ty: &DataType,
    pieces: &[(ArrayRef, Vec<u32>)],
) -> Result<ArrayRef, ExecError> {
    if let [(array, rows)] = pieces
        && rows.len() == len
    {
        return Ok(array.clone());
    }
    let null = new_null_array(&arrow_type(ty), 1);
    let mut sources: Vec<&dyn Array> = vec![null.as_ref()];
    let mut indices = vec![(0usize, 0usize); len];
    for (k, (array, rows)) in pieces.iter().enumerate() {
        sources.push(array.as_ref());
        for (j, &row) in rows.iter().enumerate() {
            indices[row as usize] = (k + 1, j);
        }
    }
    Ok(arrow_select::interleave::interleave(&sources, &indices)?)
}
