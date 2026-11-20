---
id: 0030
title: Policy Change Proposals and Distributed Verification
status: accepted  # proposal | accepted | implemented | superseded
tags: [security, policies, cedar]
feature-flag: smt-prover
created: 2026-11-19
accepted: 2026-11-19
implemented:
spec-sections: ["ir-design.md#policy-deployment-and-analysis"]
superseded-by:
---

# ADR-0030: Policy Change Proposals and Distributed Verification

## Status

Accepted

## Summary

A policy change is a proposal recorded in replicated tables. Every node can run the cheap gates on it: validation, a forbid lint, and impact analysis against its own data. Prover nodes (those configured with an SMT solver) can also prove the change, running never-errors, no-broadening and declared-invariant checks over every possible request. A cluster-wide verification mode decides what activation needs:
- **`proof-required`:** a prover's verdict, and a proposal waits until one exists.
- **`analysis-sufficient`:** the local gates, with a named confirmation for anything they flag. This is the only mode an embedded single-node database can use.

The SMT solver (cvc5) is therefore a capability of some nodes, never a requirement of the engine.

## Context

[ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md) specifies deployment gates that prove properties of a policy change over all requests, using Cedar SymCC and the cvc5 solver. cvc5 is a separate native executable, pinned to one version, and a proof can take seconds. Requiring it everywhere would make every embedded deployment ship and configure a solver. Most deployments either are a single embedded database, where nothing can prove anything, or have at least one replica on a server, which can.

Without a solver, the gates' protection can largely be recovered:
- **Validation** already rules out most runtime errors.
- **The real hazard behind never-errors** is a forbid that fails and so stops forbidding. A syntactic lint catches the error sources that remain: arithmetic, and attributes read through references.
- **Impact analysis** shows exactly what a change does to today's data.

What is lost is the guarantee over rows that do not exist yet.

## Decision

### Tables

The workflow runs entirely on ordinary tables, so it is the same on one node and, once replication exists, across many. Provers watch the tables; no new network protocol is needed.

| Table | Holds |
| --- | --- |
| `policy_set` | The active policy set: one row with its version, language (`cedar` or `rls`) and text |
| `policy_settings` | The verification mode and the Cedar type of `context.app` |
| `policy_proposals` | Each proposal: the version it is based on, its language and text, declared invariants (Cedar policies describing the most the new set may allow), the proposer, its state (`pending`, `activated`, `superseded`), and the confirmer if one was needed |
| `policy_verifications` | Each gate result: proposal, verifier, gate, passed, and detail (an impact summary, lint warnings or a counterexample) |

### Gates

| Gate | Run by | Hard? | Fails when |
| --- | --- | --- | --- |
| Validation | Every node | Yes | The policies do not parse or type-check (Cedar strict mode against the schema generated from the catalog) |
| Lint | Every node | No | A forbid condition can raise an error (arithmetic, attributes through references), so it could stop forbidding |
| Impact | Every node | No | On this node's data, some principal would gain access to a row or column it lacks today |
| Never errors | Provers | Yes | Some policy raises an error on some well-formed request |
| No broadening | Provers | No | The new set allows some request the old set denied |
| Invariants | Provers | Yes | The new set allows some request a declared invariant does not |

A failed hard gate blocks activation. A failed soft gate blocks it until a named person confirms. Failures from provers carry a concrete counterexample (a request and entities); impact failures list the affected principals and rows.

### Activation

`activate(proposal, confirmed_by)` succeeds only if:
- the proposal is pending and based on the active version;
- a passed validation result is recorded;
- in `proof-required` mode, every proof gate has a recorded result (otherwise the proposal waits for a prover);
- no hard gate failed;
- any failed soft gate is confirmed.

Activation writes the new policy set at the next version and supersedes every other pending proposal, whose base is now out of date. Reading the policy set registers a dependency ([ADR-0019](0019-transactional-authorization-and-residual-cache.md)), so concurrent activations conflict, and writers that relied on the old set fail at commit.

### Provers

A prover implements `Prover::prove(current, proposal)` and records its results with `prove_pending`.
- **The SMT prover:** built only with the engine feature `smt-prover` and given the path of a cvc5 1.3.1 executable. For each request environment of the generated schema it uses Cedar SymCC to check never-errors per policy, implication of the old row-level policies by the new ones (no broadening), and implication of the invariants by the new ones. Column-level (`@column`) policies are checked for errors only.
- **Language:** proofs need Cedar on both sides. Only Cedar policy sets can activate in `proof-required` mode.
- **No prover available:** a `proof-required` proposal waits indefinitely. Falling back to analysis alone is an explicit administrative change of mode, never automatic.

## Implementation Phases

- [x] **Phase 1: Proposal tables, local gates (validation, lint, impact analysis), activation rules, the `Prover` interface, the SMT prover** - I5
  - *As implemented:* in `interlaken-engine` (`policy` module).
    - **Tables and verification:** `create_policy_tables` creates the four tables. `submit_proposal` and `verify_locally` run validation, lint and impact analysis, impact running against an `entity_snapshot` read through the transaction.
    - **Activation:** `activate` enforces the rules above and supersedes the other pending proposals.
    - **Provers:** they implement `Prover` and record results with `prove_pending`. The cvc5-backed `SmtProver` builds only with the `smt-prover` feature.
    - *Testing:* `policy_proposals.rs` covers activation in both modes, superseding and waiting for a prover. `smt_prover.rs` covers proofs and counterexamples, and that RLS policies can't be proved; it skips if cvc5 is absent.
- [ ] **Phase 2: Provers subscribing to replicated proposal tables** - with Raft

## Consequences

### Positive
- Embedded builds never include or configure a solver.
- Clusters get proofs and counterexamples where a server can run them, and the same workflow everywhere.
- Impact analysis shows the concrete blast radius of a change on real data, which proofs alone do not.
- Policy changes are auditable: every gate result, verifier and confirmer is recorded.

### Negative
- Without a prover, guarantees cover current data only, plus whatever the lint catches syntactically.
- In `proof-required` mode, policy changes stall while no prover is reachable.
- Impact analysis evaluates every principal against every governed row, so its cost grows with the product of the two. F2 prunes it to the changed policies' actions and resource types, decides incrementally per changed policy, and samples above a threshold ([ADR-0032](0032-authorization-completeness.md)); a sampled analysis can miss changes.

## Open Questions

- Should invariants also express lower bounds (requests that must stay allowed), checked with the converse implication?

## Future Work

- Prover nodes that subscribe to proposal changes once replication exists.
- ~~Sampling and incremental impact analysis for large datasets.~~ Done in F2: [ADR-0032](0032-authorization-completeness.md).
- Checking impact under several representative contexts, such as different times and tenants.

## References

- [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0020](0020-cedar-policy-provider-and-deployment-gates.md)
- Cedar SymCC (`cedar-policy-symcc`), cvc5
