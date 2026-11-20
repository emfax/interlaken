# Privileges

Policies govern data. *Privileges* govern changes to the database's structure and policies, and visibility of policy logic. There are two:

| Privilege | Allows |
| --- | --- |
| `admin` | all DDL, marking functions leakproof, proposing, verifying and activating policies, changing policy settings, and granting and revoking privileges. Implies `read-policies`. |
| `read-policies` | seeing policy logic in `EXPLAIN` |

An operation that needs `admin`, run by a principal without it, fails with `42501`. The principal passed to `Engine::create` is the first administrator.

Privileges don't bypass policies. An administrator's queries are governed like anyone else's, and an administrator may read and write only what the active policies allow. An administrator who controls the policies can, of course, grant themselves access through a policy change, which the verification gates and the proposals table record (see [Changing policies](proposals.md)).

## Granting and revoking

```rust
# use interlaken_engine::{Engine, EngineOptions, Privilege};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::{DataType, ScalarType};
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
let admin = EntityUid::long("User", 1);
let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
let mut session = engine.session(SecurityContext::new(admin));

let auditor = EntityUid::long("User", 7);
session.grant(&auditor, Privilege::ReadPolicies).await?;

let mut theirs = engine.session(SecurityContext::new(auditor.clone()));
assert!(theirs.privileges().await?.read_policies);
assert!(!theirs.privileges().await?.admin);

assert!(session.revoke(&auditor, Privilege::ReadPolicies).await?);
# Ok(())
# }
```

Privileges are granted to principals by entity UID, and the principal needn't exist as a row. They are stored in the `engine_privileges` table and read through each statement's transaction:
- **Granting:** a grant takes effect for transactions that start after it commits.
- **Revoking:** a revocation committed concurrently with a writing SERIALIZABLE transaction that relied on the privilege makes that transaction fail at commit with `40001`.

`revoke` returns whether the principal held the privilege. Granting a privilege twice is harmless.

**The last administrator stays.** Revoking `admin` from the only principal holding it fails with `55000`, since no one could then run DDL, change policies or grant privileges again. Grant `admin` to another principal first. Two administrators revoking each other in concurrent transactions can't get around this: one of the commits fails with `40001`, at every isolation level.

## Leakproof functions

The optimizer may evaluate a predicate below a policy filter only if every function in it is *leakproof*. A leakproof function can't raise an error or otherwise reveal anything about its arguments. Evaluating a non-leakproof function on rows the principal can't see could leak them through errors: a query that divides by `(- secret 42)` fails exactly when a hidden row's secret is 42. So the optimizer keeps such predicates above the barrier, where they see only visible rows.

No built-in function is leakproof by default, since each needs an audit first. An administrator who has audited one can mark it, so that predicates using it can be pushed below barriers and into index scans:

```rust,ignore
session
    .set_function_leakproof("lower", &[DataType::Scalar(ScalarType::Text)], true)
    .await?;
```

The overload is named by its argument types; one that doesn't exist fails with `42P16`. Marking a function leakproof changes the schema version, like DDL. Comparisons, `NOT`, `IS [NOT] NULL` and `IS [NOT] DISTINCT FROM` are always leakproof. Arithmetic, casts that can fail, `LIKE` and concatenation never are.

Mark a function leakproof only if it really can't fail on any input. Getting this wrong weakens every policy.
