//! Per-column sort order: direction and NULL placement.

/// Sort direction of a key column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Ascending.
    Asc,
    /// Descending: every byte of the column's encoding is inverted.
    Desc,
}

/// Where NULLs sort relative to non-NULL values, in final key order.
///
/// The placement is absolute: it does not flip with [`Direction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NullOrder {
    /// NULL sorts before every value.
    First,
    /// NULL sorts after every value.
    Last,
}

/// The sort order of one key column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SortOrder {
    /// Ascending or descending.
    pub direction: Direction,
    /// NULL placement.
    pub nulls: NullOrder,
}

impl SortOrder {
    /// Postgres default for ascending columns: `ASC NULLS LAST`.
    pub const ASC: Self = Self::new(Direction::Asc, NullOrder::Last);

    /// Postgres default for descending columns: `DESC NULLS FIRST`.
    pub const DESC: Self = Self::new(Direction::Desc, NullOrder::First);

    /// Construct a sort order from a direction and NULL placement.
    #[must_use]
    pub const fn new(direction: Direction, nulls: NullOrder) -> Self {
        Self { direction, nulls }
    }
}

impl Default for SortOrder {
    /// Return [`SortOrder::ASC`], the Postgres default.
    fn default() -> Self {
        Self::ASC
    }
}
