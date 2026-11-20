# The statement pipeline

Every query and DML statement takes the same path from text to rows, inside one transaction: the session's open transaction, or its own in autocommit.

| Step | What happens | Fails with |
| --- | --- | --- |
| 1. Parse | the IR text becomes an unresolved plan; a plan built in Rust skips this | `42601` |
| 2. Load the catalog | the catalog as the transaction sees it, at its schema version | |
| 3. Read privileges | the principal's privileges, read through the transaction | |
| 4. Resolve | names become catalog IDs and bindings, every expression is typed, and subqueries are lowered; a plan already resolved is kept | `42P01`, `42703`, `42804`, … |
| 5. Verify | the plan is checked as untrusted client IR: no privileged operators, no writes to catalog tables, and pinned to the current schema version | `42501`, `0A000` |
| 6. Load policies | the active policy set and its settings, and the policy provider for them | |
| 7. Load entities | the principal's attributes and groups, and every other entity the policies read | |
| 8. Secure | the policies are partially evaluated into residuals and lowered into the plan as filters, write checks and masks | `42501` |
| 9. Optimize | rule-based rewriting: simplification, decorrelation, pushdown that respects security barriers, pruning | |
| 10. Plan physically | operators and access paths are chosen: index or full scans, join algorithms, top-N | |
| 11. Execute | the physical plan runs under the statement's savepoint, memory budget and timeout | the execution error |

Steps 1–10 *prepare* the statement. Every step after parsing reads through the statement's transaction, so the catalog, privileges, policies and entity data a statement is planned with come from the same snapshot as the rows it reads. A concurrent change to any of them is a conflict like any other (see [Authorization is transactional](../security/model.md#authorization-is-transactional)).

`Session::explain` stops after step 10 and prints the physical plan; with `analyze`, it also runs step 11. `Session::prepare` stops after step 5 and returns the resolved plan. The statement timeout covers every step from step 2 on.

## Caches

Most of preparation is cached, so a statement that has run before costs little more than its execution:

| Cache | Holds | Keyed by |
| --- | --- | --- |
| catalog | the loaded catalogs of the four newest schema versions | schema version |
| policy provider | the provider built for the newest policies: the generated Cedar schema and the parsed, validated policies | schema version, policy version, the policy text and the type of `context.app` |
| residuals | each residual a provider returned | principal, action, resource type, policy version, a fingerprint of the entity data the policies read, and the context values they read (see [Partial evaluation and the residual cache](../security/principals.md#partial-evaluation-and-the-residual-cache)) |

Each key includes everything the cached value depends on, so a cache never returns a stale value. In particular, the transaction reads the schema version and policy version through itself before using the caches, so a statement uses the catalog and policies its own snapshot sees.

The caches are bypassed by a transaction that has run DDL or changed policies. Its uncommitted catalog or policies aren't the committed ones the caches hold, and another transaction may have reached the same version numbers with different changes. Such a transaction loads the catalog, builds a provider and partially evaluates policies afresh for each statement, until it commits or rolls back.

When no entity type is declared, no table is governed, and step 8 adds nothing.

## Prepared plans

`Session::prepare(text)` resolves and verifies a statement and returns the resolved plan, pinned to the current schema version. `Session::query_plan(&plan, params)` runs it, skipping steps 1 and 4 (see [Parameters and prepared plans](../ir/parameters.md#prepared-plans)).

A prepared plan isn't secured. Steps 6 to 11 run on every execution, because they depend on the principal, the policies and the data, which can change between executions. So one prepared plan can be shared by sessions of different principals, and each is governed by the policies current in its own transaction.

DDL increments the schema version. A plan pinned to an older version fails verification with `0A000`, since the tables and columns it refers to may have changed; prepare it again.

## DDL and policy changes

DDL, policy and privilege methods (`create_table`, `submit_policies`, `grant` and the others) run as statements too, with the same atomicity, in the same transactions. They take steps 2 and 3, check that the principal holds `admin`, and then write the catalog or policy tables. After a DDL statement, the engine reloads the catalog within the statement to check that the changed declarations still fit together, so an invalid combination fails the statement rather than a later one.
