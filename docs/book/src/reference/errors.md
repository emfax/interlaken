# Error codes

Every error the engine returns has a Postgres SQLSTATE code (`EngineError::sqlstate`). Codes keep their Postgres meaning, so clients can treat them the same way. In particular, `40001` (serialization failure) means the transaction may succeed if retried.

This table is generated from `interlaken_engine::SQLSTATES`. A test checks that the table lists exactly the codes the engine, executor and interpreter can return.

<!-- begin generated: sqlstates -->
| Code | Condition | When |
| --- | --- | --- |
| `08P01` | `protocol_violation` | A statement was run with a different number of parameter values than the parameters it declares. |
| `0A000` | `feature_not_supported` | The statement uses something not supported yet, or a prepared plan pinned to an older schema version ran after DDL changed it. |
| `21000` | `cardinality_violation` | A scalar subquery returned more than one row. |
| `22003` | `numeric_value_out_of_range` | Arithmetic overflowed, or a value does not fit its target type. |
| `22004` | `null_value_not_allowed` | An entity attribute a policy needs is absent (`assert_present`), or NULL was passed for a parameter declared NOT NULL. |
| `22011` | `substring_error` | `substr` was called with a negative length. |
| `22012` | `division_by_zero` | Division or modulo by zero. |
| `2201W` | `invalid_row_count_in_limit_clause` | A `LIMIT` is negative. |
| `2201X` | `invalid_row_count_in_result_offset_clause` | An `OFFSET` is negative. |
| `22025` | `invalid_escape_sequence` | A `LIKE` pattern ends with an escape character. |
| `22P02` | `invalid_text_representation` | Text cast to another type is not a valid value of it. |
| `23502` | `not_null_violation` | A NULL was written to a NOT NULL column. |
| `23505` | `unique_violation` | A row duplicates another's primary key or unique index key. |
| `23514` | `check_violation` | A membership edge written by DML would make its hierarchy cyclic. |
| `25001` | `active_sql_transaction` | `begin` was called with a transaction already open. |
| `25006` | `read_only_sql_transaction` | A read-only transaction tried to write. |
| `25P01` | `no_active_sql_transaction` | A transaction operation was called with no transaction open. |
| `25P02` | `in_failed_sql_transaction` | An earlier statement in the transaction failed; only a rollback (to a savepoint or entirely) is accepted. |
| `25P04` | `transaction_timeout` | The transaction stayed open longer than `StorageOptions::max_transaction_age`; versions it needs may have been garbage-collected, so it can't continue. |
| `40001` | `serialization_failure` | The transaction conflicted with a concurrent one, detected when it commits; retry it. |
| `42000` | `syntax_error_or_access_rule_violation` | The plan failed resolution or verification for a reason without a more specific code. |
| `42501` | `insufficient_privilege` | The principal may not do this: a column its policies deny, a row its write checks reject, a masked column a write can't carry, an operation that needs a privilege it lacks, or a privileged operator or system table in a client plan. |
| `42601` | `syntax_error` | The IR text, or stored policy text, does not parse. |
| `42701` | `duplicate_column` | A table already has a column of that name. |
| `42702` | `ambiguous_column` | A column reference matches more than one input column. |
| `42703` | `undefined_column` | A column does not exist. |
| `42704` | `undefined_object` | An index does not exist. |
| `42725` | `ambiguous_function` | A function call matches more than one overload equally well. |
| `42804` | `datatype_mismatch` | An expression has the wrong type for where it is used, or a parameter value is not of its declared type. |
| `42883` | `undefined_function` | No function of that name accepts the argument types. |
| `42P01` | `undefined_table` | A table does not exist. |
| `42P07` | `duplicate_table` | A table or index of that name already exists. |
| `42P16` | `invalid_table_definition` | A DDL definition is invalid: an empty or reserved name, a type tables can't hold, a table that backs an entity type, a declaration that is missing, duplicated or doesn't fit the others, or a function overload that doesn't match exactly. |
| `53200` | `out_of_memory` | An operator needed more memory than the query's budget or the node's pool allows. |
| `54000` | `program_limit_exceeded` | The transaction's buffered writes exceed `StorageOptions::max_write_set_bytes`. |
| `55000` | `object_not_in_prerequisite_state` | A policy proposal is missing, not pending, stale, unverified, awaiting a prover, failed a deployment gate or needs confirmation. |
| `57014` | `query_canceled` | The statement ran past its `statement_timeout`. |
| `XX000` | `internal_error` | An internal error: a storage failure, or the stored catalog disagrees with itself. |
<!-- end generated: sqlstates -->
