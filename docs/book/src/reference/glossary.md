# Glossary

**Aborted transaction:** An explicit transaction in which a statement failed. It accepts only a rollback, to a savepoint or entirely; anything else fails with `25P02`. See [Statements, atomicity and savepoints](../transactions/statements.md).

**Action:** What a policy governs: `read`, `insert`, `update` or `delete`. See [The security model](../security/model.md#policies).

**Apply:** A dependent join: for each row of its left input, it evaluates its right input with that row's values. Subqueries are lowered to `apply`, and the optimizer turns most into ordinary joins. See [Relational operators](../ir/operators.md#apply).

**Autocommit:** How a session runs a statement when no transaction is open: in a transaction of its own, committed if the statement succeeds.

**Closure table:** A table holding every (descendant, ancestor) pair of a membership hierarchy, with a count of the paths between them, maintained by the engine whenever the edges change. It makes a membership test one lookup, however deep the hierarchy. See [Entities and memberships](../ddl/entities.md#memberships).

**Column policy:** A policy deciding whether a principal may read a column, per row, optionally with a *mask* to show instead. See [Column policies and masks](../security/masks.md).

**Commit group:** The commits the commit sequencer validates and writes together, with one write and, under `Durability::Sync`, one sync. See [Durability](../operations/durability.md#group-commit).

**Entity, entity type:** A row of a table declared as an entity type, which policies see as an entity with attributes, such as `User::"7"` with `dept`. Declaring an entity type *governs* its table. See [Entities and memberships](../ddl/entities.md).

**Governed table:** A table backing an entity type. Policies decide which of its rows each principal may read and write, and with no permitting policy, the answer is none. Other tables are ungoverned and open to every principal. See [Governed tables](../security/model.md#governed-tables).

**IR (intermediate representation):** The typed relational algebra every statement is expressed in, and its S-expression text form, the IR text language. See [Syntax and names](../ir/syntax.md).

**Leakproof:** A function or expression that can't raise an error or otherwise reveal anything about its inputs. Only leakproof predicates may be evaluated below a policy filter. Administrators mark functions leakproof. See [Leakproof functions](../security/privileges.md#leakproof-functions).

**Mask:** The value a column policy shows in place of a column's value on rows the principal may not read it, such as NULL or the result of `mask_last4`. See [Column policies and masks](../security/masks.md).

**Membership:** A declared relation "member *in* group" between entity types, stored as an edges table and a closure table. Policies test it with `in`. See [Entities and memberships](../ddl/entities.md#memberships).

**Partial evaluation:** Evaluating policies with everything known before a statement runs (the principal, its attributes and memberships, and the request context), leaving only the conditions on the row: the *residual*. See [Partial evaluation and the residual cache](../security/principals.md#partial-evaluation-and-the-residual-cache).

**Pinned plan:** A resolved plan, tied to the schema version it was resolved against. `Session::prepare` returns one, and it fails with `0A000` after DDL changes the schema version. See [Prepared plans](../ir/parameters.md#prepared-plans).

**Policy filter:** The operator security lowering puts above every read of a governed table, applying the `read` residual. It is a *security barrier*: only leakproof predicates may move below it, so no other expression sees a row the principal may not read. See [How policies become plans](../security/model.md#how-policies-become-plans).

**Policy provider:** The component that turns a policy language into residuals. Interlaken has a Cedar provider and an RLS provider. See [Writing policies in Cedar](../security/cedar.md) and [Writing policies in the RLS sublanguage](../security/rls.md).

**Policy set, policy version:** The active policies, in one language, and their version number, which each activation increments. Every statement uses the version current in its transaction.

**Principal:** The authenticated user or service a session acts for, identified by an `EntityUid` such as `User::"7"`. See [Principals and request context](../security/principals.md).

**Privilege:** A right to change the database's structure or policies (`admin`) or to see policy logic in `EXPLAIN` (`read-policies`). Privileges don't bypass policies. See [Privileges](../security/privileges.md).

**Proposal:** A submitted policy change. Verification gates check it, and an administrator activates it, confirming anything the gates flag. See [Changing policies](../security/proposals.md).

**Prover node:** A build with the `smt-prover` feature and the cvc5 solver, which can prove properties of Cedar policy changes for every possible request. See [Changing policies](../security/proposals.md).

**Request context:** The facts about a request that policies may read besides the principal: `context.time`, `context.ip`, `context.auth_method`, `context.session_id` and application fields under `context.app`. See [The security context](../security/principals.md#the-security-context).

**Residual:** What remains of the policies for one principal, action and table after partial evaluation: always true, always false, or a condition over the row. It is cached in the *residual cache*.

**Resolved, unresolved:** The two forms of an IR plan: with names, as written, or with catalog IDs, bindings and types, after the resolver. See [Unresolved and resolved forms](../ir/syntax.md#unresolved-and-resolved-forms).

**Savepoint:** A point in an explicit transaction to roll back to, undoing later statements. Each statement also runs under an implicit savepoint, which is how a failed statement is undone. See [Savepoints](../transactions/statements.md#savepoints).

**Schema version:** A catalog-wide counter that every DDL statement increments. Each statement reads it, so DDL conflicts with concurrent writers and stale pinned plans are rejected. See [Transactions and concurrency](../ddl/tables.md#transactions-and-concurrency).

**Security context:** A session's principal and request context, supplied by the application, which the engine trusts and clients can't change.

**Security lowering:** The pass that turns residuals into plan operators: policy filters, write checks, filters on the rows updates and deletes start from, and masks.

**Session:** A connection's worth of statements, run for one principal, in autocommit or in explicit transactions. See [Sessions and principals](../getting-started/sessions.md).

**Snapshot:** The committed state a transaction reads: as of its start under REPEATABLE READ and SERIALIZABLE, refreshed each statement under READ COMMITTED. See [Transactions and isolation](../transactions/isolation.md).

**Statement:** One query, DML statement, DDL operation or policy change. Each is atomic: if it fails, none of its writes are kept.

**Verification gate:** A check a policy proposal must pass before activation: validation, the forbid lint, impact analysis, and on prover nodes, proofs. See [Changing policies](../security/proposals.md).

**Verifier:** The pass every plan from a client goes through, rejecting privileged operators, writes to catalog tables and plans pinned to an old schema version. See [What clients can't write](../ir/syntax.md#what-clients-cant-write).

**Watermark:** The oldest snapshot an open transaction holds. Version GC removes only versions that no snapshot at or after the watermark can read. See [Garbage collection and disk space](../operations/gc.md).

**Write check:** The operator security lowering puts on every insert and update, checking each written row against the action's policies. A row that fails it fails the statement with `42501`. See [Writes and write checks](../security/writes.md).

**Write set:** The writes a transaction has buffered and not yet committed, limited by `StorageOptions::max_write_set_bytes`.
