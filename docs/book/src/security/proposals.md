# Changing policies: proposals, verification and impact analysis

Policies decide who can see what, so a careless change can expose data or lock everyone out. Interlaken never changes policies in place. A change is a *proposal*: it is verified by gates and activated by an administrator, and every step is recorded.

## The workflow

```rust
# use interlaken_engine::catalog::{AttributeSpec, ColumnSpec, EntitySpec, TableSpec};
# use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
# use interlaken_engine::{Engine, EngineOptions};
# use interlaken_ir::security::{EntityUid, SecurityContext};
# use interlaken_ir::text::parse_type;
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
# let admin = EntityUid::long("User", 1);
# let engine = Engine::create(dir.path(), EngineOptions::default(), &admin).await?;
# let mut session = engine.session(SecurityContext::new(admin));
# let table = |name: &str, columns: &[(&str, &str)]| TableSpec {
#     name: name.into(),
#     columns: columns.iter().map(|(n, t)| ColumnSpec::new(*n, parse_type(t).unwrap())).collect(),
#     primary_key: vec!["id".into()],
# };
# session.create_table(&table("users", &[("id", "int8"), ("dept", "(option text)")])).await?;
# session.create_table(&table("docs", &[("id", "int8"), ("owner", "(option int8)")])).await?;
# session.declare_entity(&EntitySpec { name: "User".into(), table: "users".into(), key: "id".into(), attributes: vec![AttributeSpec::new("dept")] }).await?;
# session.declare_entity(&EntitySpec { name: "Doc".into(), table: "docs".into(), key: "id".into(), attributes: vec![AttributeSpec::reference("owner", "owner", "User")] }).await?;
let document = PolicyDocument {
    language: PolicyLanguage::Rls,
    body: r#"(policies
        (permit (read insert update delete) User (== resource principal))
        (permit (read insert update delete) Doc (== resource.owner principal)))"#
        .into(),
};
// 1. Propose: record the change, based on the active policy version.
let proposal = session.submit_policies(&document, "").await?;
// 2. Verify: run the local gates and record their results.
for gate in session.verify_policies(proposal).await? {
    println!("{:?}: passed={} {}", gate.gate, gate.passed, gate.detail);
}
// 3. Activate: make it the active policy set, confirming any flagged gate.
let version = session.activate_policies(proposal, true).await?;
println!("policies are now at version {version}");
# Ok(())
# }
```

All three steps need the `admin` privilege, and each is a statement that can run in a transaction like any other.

| Step | Method | Does |
| --- | --- | --- |
| propose | `submit_policies(document, invariants)` | records the new policy set, its language, optional Cedar invariants and the proposer; returns the proposal's ID |
| verify | `verify_policies(proposal)` | runs the local gates, records each result under the verifying principal, and returns them |
| activate | `activate_policies(proposal, confirm)` | checks the activation rules, writes the new policy set at the next version, and returns that version |

A proposal names the whole new policy set, not a diff.

## Gates

| Gate | Run by | Blocks activation | Fails when |
| --- | --- | --- | --- |
| Validation | every node | always | the policies don't parse or type-check. Cedar is validated in strict mode against the schema generated from the catalog. |
| Lint | every node | until confirmed | a forbid condition can raise an error, and so could stop forbidding |
| Impact | every node | until confirmed | on the current data, some principal would gain access to a row or column they lack today |
| Never errors | prover nodes | always | some policy raises an error on some well-formed request |
| No broadening | prover nodes | until confirmed | the new set allows some request the old one denies |
| Invariants | prover nodes | always | the new set allows some request a declared invariant doesn't |

Impact analysis compares the old and new policy sets on today's data. It treats each entity in the entity tables as a principal, and decides each read and write on each governed row and column, using the verifying session's request context. Its result lists what becomes newly allowed and newly denied:

```text
Impact: passed=false  405 decisions compared; 6 newly allowed [User::"2" read Doc::"100", ...];
        9 newly denied [User::"1" read column Doc.title, ...]
```

It looks only at what the change touches: the actions and resource types of the policies that differ. Above a million decisions, it samples principals and rows, and says so in its result. A sampled analysis can miss changes.

The prover gates need Cedar policies and a node built with the `smt-prover` feature and configured with the cvc5 solver. They prove their property for every possible request and entity, not just today's data, or record a counterexample.

## Activation rules

`activate_policies` succeeds only if:

- the proposal is pending, and based on the policy version that is active now;
- a passed validation result is recorded;
- no gate that always blocks has failed;
- every failed gate that blocks until confirmed is confirmed: passing `confirm: true` records the activating principal as the confirmer;
- in `proof-required` mode, every prover gate has a recorded result.

Otherwise it fails with `55000` and says why. Activating a proposal supersedes every other pending proposal, since their base version is no longer current. Concurrent activations conflict, and transactions that wrote while relying on the old policies fail at commit with `40001` (see [Authorization is transactional](model.md#authorization-is-transactional)).

## Policy settings

`set_policy_settings` changes two cluster-wide settings, which `EngineOptions::policy_settings` sets for a new database:

- **The verification mode:**
  - `VerificationMode::AnalysisSufficient`, the default, activates on the local gates, with confirmations for anything they flag. It is the only mode an embedded database without a prover can use.
  - `VerificationMode::ProofRequired` makes every proposal wait for a prover's results. It needs Cedar policies.
- **The application context type:** the Cedar record type of `context.app`, such as `{ "tenant": String }`, which Cedar policies are validated against. It defaults to `{}`, an empty record.

```rust,ignore
use interlaken_engine::policy::{PolicySettings, VerificationMode};

session
    .set_policy_settings(&PolicySettings {
        mode: VerificationMode::AnalysisSufficient,
        app_context: r#"{ "tenant": String }"#.into(),
    })
    .await?;
```

Changing the settings needs `admin`.

## The audit trail

Proposals, gate results, verifiers and confirmers are kept in the engine's policy tables (see [Catalog tables](../reference/catalog-tables.md)). Clients can't query those tables. The `interlaken_engine::policy` module reads them through a storage transaction: `pending_proposals`, `proposal`, `verifications` and `load_policies`.
