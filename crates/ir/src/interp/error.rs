//! Errors raised while evaluating a plan, with their SQLSTATE codes.

/// An error raised while evaluating a plan.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// A value does not fit its type (SQLSTATE 22003).
    #[error("{0} out of range")]
    OutOfRange(String),
    /// Division or modulo by zero (22012).
    #[error("division by zero")]
    DivisionByZero,
    /// Text that does not parse as the target type (22P02).
    #[error("invalid input syntax for type {ty}: \"{text}\"")]
    InvalidText {
        /// The target type.
        ty: String,
        /// The text.
        text: String,
    },
    /// A scalar subquery returned more than one row (21000).
    #[error("more than one row returned by a subquery used as an expression")]
    CardinalityViolation,
    /// A `LIKE` pattern ends with the escape character (22025).
    #[error("LIKE pattern must not end with escape character")]
    InvalidEscape,
    /// A negative `substr` length (22011).
    #[error("negative substring length not allowed")]
    NegativeSubstringLength,
    /// A negative `LIMIT` (2201W) or `OFFSET` (2201X).
    #[error("{0} must not be negative")]
    NegativeLimit(&'static str),
    /// NULL stored in a NOT NULL column (23502).
    #[error(
        "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"
    )]
    NotNull {
        /// The table.
        table: String,
        /// The column.
        column: String,
    },
    /// A row refers to a row that doesn't exist through a foreign key (23503).
    #[error("foreign key violation: {0}")]
    ForeignKey(String),
    /// A duplicate primary key or unique index key (23505).
    #[error("duplicate key value violates a unique constraint of \"{0}\"")]
    UniqueViolation(String),
    /// A required value is absent: `assert_present` of NULL, used for
    /// policy attributes that are not set (22004).
    #[error("attribute is absent")]
    Absent,
    /// A row failed a `WriteCheck` (42501).
    #[error("new row violates the security policy")]
    PermissionDenied,
    /// The plan uses something the interpreter cannot evaluate (0A000);
    /// for a verified plan this indicates an interpreter gap.
    #[error("not supported by the reference interpreter: {0}")]
    Unsupported(String),
}

impl EvalError {
    /// Return the Postgres SQLSTATE code for this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::interp::EvalError;
    ///
    /// assert_eq!(EvalError::DivisionByZero.sqlstate(), "22012");
    /// ```
    #[must_use]
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::OutOfRange(_) => "22003",
            Self::DivisionByZero => "22012",
            Self::InvalidText { .. } => "22P02",
            Self::CardinalityViolation => "21000",
            Self::InvalidEscape => "22025",
            Self::NegativeSubstringLength => "22011",
            Self::NegativeLimit("LIMIT") => "2201W",
            Self::NegativeLimit(_) => "2201X",
            Self::NotNull { .. } => "23502",
            Self::UniqueViolation(_) => "23505",
            Self::ForeignKey(_) => "23503",
            Self::Absent => "22004",
            Self::PermissionDenied => "42501",
            Self::Unsupported(_) => "0A000",
        }
    }
}
