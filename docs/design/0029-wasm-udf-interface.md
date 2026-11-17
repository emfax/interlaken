---
id: 0029
title: WASM UDF Interface
status: proposal  # proposal | accepted | implemented | superseded
tags: [executor, wasm, udf]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections: ["executor-design.md#wasm-udf-interface-designed-now-built-later"]
superseded-by:
---

# ADR-0029: WASM UDF Interface

## Status

Proposal

## Summary

WASM user-defined functions are called by a `UdfProject` operator once per batch, with columns copied into guest linear memory in Arrow layout. Guests run on wasmtime with async support, CPU and memory limits, and cancellation tied to the query stream. Function properties come from the catalog; only deterministic functions may run in read-write transactions. The interface is designed now and implemented with the WASM runtime.

## Context

WASM extensibility is deferred, but the executor's batch format and operator model must not preclude it. Per-row calls across the WASM boundary would dominate cost. Guests may call back into the host (including storage), which must not block executor threads. Untrusted code needs resource limits, and its properties (volatility, leakproofness, determinism) affect optimization and security ([ADR-0011](0011-function-property-catalog.md)). Transactions can be retried, so non-deterministic code in read-write transactions is unsafe.

## Decision

- **Batch calls.** A `UdfProject` operator calls a WASM function once per batch, with columns copied into guest linear memory in Arrow layout (once per batch).
- **Async calls.** wasmtime's async support suspends the guest while it awaits host calls, so no executor thread blocks.
- **Limits.** Epoch interruption for CPU (it ties into `statement_timeout` and cancellation), memory caps per instance, and cancellation tied to the query's stream ([ADR-0022](0022-vectorized-arrow-stream-executor.md)).
- **Determinism and properties.** Only functions declared deterministic may run in read-write transactions. Volatility and leakproofness come from the catalog, never from client-supplied IR.
- **Pooling.** Instances come from wasmtime's pooling allocator, one per query, never shared across transactions.
- **Host queries.** WASM host calls never get raw storage access; their scans go through the same planner path and carry the caller's security context ([ADR-0025](0025-runtime-security-enforcement-in-executor.md)).

## Implementation Phases

- [ ] **Phase 1: Model function properties in the catalog** - I2
- [ ] **Phase 2: UdfProject and wasmtime integration** - with the WASM runtime (post-MVP)

## Consequences

### Positive
- Per-batch calls amortise boundary-crossing cost.
- Arrow layout reuses the executor's batch format directly.
- Isolation per transaction and strict limits contain untrusted code.

### Negative
- Columns are copied into guest memory on every call.
- Determinism and leakproofness are declared, not verified; correctness depends on administrators marking functions accurately.

## Open Questions

- What WIT interface do guests see for Arrow batches and host calls?

## Future Work

- The WASM runtime, host functions mapped onto `Txn` methods, and WASM query builders.
- Fuel metering where deterministic CPU accounting is needed.

## References

- `executor-design.md` — WASM UDF interface
- [ADR-0001](0001-fjall-storage-engine-and-async-sync-boundary.md), [ADR-0006](0006-batched-async-storage-trait.md), [ADR-0011](0011-function-property-catalog.md), [ADR-0013](0013-ir-forms-formats-resolver-and-verifier.md), [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0025](0025-runtime-security-enforcement-in-executor.md)
- wasmtime: async support, pooling allocator, fuel and epoch interruption
