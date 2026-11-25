//! The SQLSTATE codes the engine returns, with what each means here.
//!
//! [`EngineError::sqlstate`](crate::EngineError::sqlstate) maps every error
//! to one of these codes. The table documents them; a test checks it lists
//! exactly the codes the `sqlstate` methods of the engine, executor,
//! interpreter and storage can return, and the book's error reference is generated
//! from it.

/// One SQLSTATE code the engine returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlState {
    /// The five-character code.
    pub code: &'static str,
    /// Postgres's condition name for the code.
    pub name: &'static str,
    /// When Interlaken returns it.
    pub meaning: &'static str,
}

/// Construct a table entry.
const fn state(code: &'static str, name: &'static str, meaning: &'static str) -> SqlState {
    SqlState {
        code,
        name,
        meaning,
    }
}

/// Every SQLSTATE code the engine returns, in code order.
pub const SQLSTATES: &[SqlState] = &[
    state(
        "08007",
        "transaction_resolution_unknown",
        "A commit may or may not have happened: writing it failed, or a replicated database's leader changed while it was in flight.",
    ),
    state(
        "08P01",
        "protocol_violation",
        "A statement was run with a different number of parameter values than the parameters it declares.",
    ),
    state(
        "0A000",
        "feature_not_supported",
        "The statement uses something not supported yet, or a prepared plan pinned to an older schema version ran after DDL changed it, or DDL was given to `prepare`, `explain` or `query_stream`.",
    ),
    state(
        "21000",
        "cardinality_violation",
        "A scalar subquery returned more than one row.",
    ),
    state(
        "22003",
        "numeric_value_out_of_range",
        "Arithmetic overflowed, or a value does not fit its target type.",
    ),
    state(
        "22004",
        "null_value_not_allowed",
        "An entity attribute a policy needs is absent (`assert_present`), or NULL was passed for a parameter whose type isn't optional.",
    ),
    state(
        "22011",
        "substring_error",
        "`substr` was called with a negative length.",
    ),
    state("22012", "division_by_zero", "Division or modulo by zero."),
    state(
        "2201W",
        "invalid_row_count_in_limit_clause",
        "A `LIMIT` is negative.",
    ),
    state(
        "2201X",
        "invalid_row_count_in_result_offset_clause",
        "An `OFFSET` is negative.",
    ),
    state(
        "22025",
        "invalid_escape_sequence",
        "A `LIKE` pattern ends with an escape character.",
    ),
    state(
        "22P02",
        "invalid_text_representation",
        "Text cast to another type is not a valid value of it.",
    ),
    state(
        "23502",
        "not_null_violation",
        "A NULL was written to a NOT NULL column: one whose type isn't optional.",
    ),
    state(
        "23503",
        "foreign_key_violation",
        "A row refers through a foreign key to a row that doesn't exist, or a deleted or changed row is still referred to.",
    ),
    state(
        "23505",
        "unique_violation",
        "A row duplicates another's primary key or unique index key.",
    ),
    state(
        "23514",
        "check_violation",
        "A membership edge written by DML would make its hierarchy cyclic.",
    ),
    state(
        "25001",
        "active_sql_transaction",
        "`begin` was called with a transaction already open.",
    ),
    state(
        "25006",
        "read_only_sql_transaction",
        "A read-only transaction tried to write.",
    ),
    state(
        "25P01",
        "no_active_sql_transaction",
        "A transaction operation was called with no transaction open.",
    ),
    state(
        "25P02",
        "in_failed_sql_transaction",
        "An earlier statement in the transaction failed; only a rollback (to a savepoint or entirely) is accepted.",
    ),
    state(
        "25P04",
        "transaction_timeout",
        "The transaction stayed open longer than `StorageOptions::max_transaction_age`; versions it needs may have been garbage-collected, so it can't continue.",
    ),
    state(
        "40001",
        "serialization_failure",
        "The transaction conflicted with a concurrent one, detected when it commits; retry it.",
    ),
    state(
        "42000",
        "syntax_error_or_access_rule_violation",
        "The plan failed resolution or verification for a reason without a more specific code.",
    ),
    state(
        "42501",
        "insufficient_privilege",
        "The principal may not do this: a column its policies deny, a row its write checks reject, a masked column a write can't carry, an operation that needs a privilege it lacks, or a privileged operator or system table in a client plan.",
    ),
    state(
        "42601",
        "syntax_error",
        "The IR text, or stored policy text, does not parse.",
    ),
    state(
        "42701",
        "duplicate_column",
        "A table already has a column of that name.",
    ),
    state(
        "42702",
        "ambiguous_column",
        "A column reference matches more than one input column.",
    ),
    state("42703", "undefined_column", "A column does not exist."),
    state("42704", "undefined_object", "An index does not exist."),
    state(
        "42725",
        "ambiguous_function",
        "A function call matches more than one overload equally well.",
    ),
    state(
        "42804",
        "datatype_mismatch",
        "An expression has the wrong type for where it is used, or a parameter value is not of its declared type.",
    ),
    state(
        "42883",
        "undefined_function",
        "No function of that name accepts the argument types.",
    ),
    state("42P01", "undefined_table", "A table does not exist."),
    state(
        "42P07",
        "duplicate_table",
        "A table or index of that name already exists.",
    ),
    state(
        "42P16",
        "invalid_table_definition",
        "A DDL definition is invalid: an empty or reserved name, a type tables can't hold, a table that backs an entity type, a declaration that is missing, duplicated or doesn't fit the others, or a function overload that doesn't match exactly.",
    ),
    state(
        "53200",
        "out_of_memory",
        "An operator needed more memory than the query's budget or the node's pool allows.",
    ),
    state(
        "54000",
        "program_limit_exceeded",
        "The transaction's buffered writes exceed `StorageOptions::max_write_set_bytes`.",
    ),
    state(
        "55000",
        "object_not_in_prerequisite_state",
        "A policy proposal is missing, not pending, stale, unverified, awaiting a prover, failed a deployment gate or needs confirmation; a revocation would remove the last administrator; or a change feed's position is older than the change log keeps, or the database keeps none.",
    ),
    state(
        "57014",
        "query_canceled",
        "The statement ran past its `statement_timeout`, or a wait for a commit to become visible on this node timed out.",
    ),
    state(
        "57P03",
        "cannot_connect_now",
        "A replicated database can't commit now: it has no leader, or the leader can't be reached. The transaction didn't commit.",
    ),
    state(
        "XX000",
        "internal_error",
        "An internal error: a storage failure, or the stored catalog disagrees with itself.",
    ),
];
