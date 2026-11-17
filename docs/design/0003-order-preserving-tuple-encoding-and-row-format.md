---
id: 0003
title: Order-Preserving Tuple Encoding and Row Format
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, encoding]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#order-preserving-tuple-encoding
  - mvp-fjall-storage-design.md#row-and-value-format
superseded-by:
---

# ADR-0003: Order-Preserving Tuple Encoding and Row Format

## Status

Proposal

## Summary

Keys are encoded as self-delimiting tuples in which each column is a type tag followed by a type-specific body, so byte order equals logical value order (including DESC columns and Postgres NULL placement). Row values use a separate, non-order-preserving format: schema version ID, null bitmap, fixed-width columns, then variable-length columns.

## Context

fjall orders keys by raw bytes. Range scans, index ordering and the versioned key layout ([ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md)) all require that the byte order of an encoded key matches the logical order of its column values, and that a tuple is self-delimiting so the version suffix can follow it directly. Row values, by contrast, don't need ordering but do need to survive schema evolution.

## Decision

### Tuple (key) encoding

Each column is a type tag followed by a type-specific body.

| Type | Encoding |
| --- | --- |
| NULL | One tag byte: low for NULLS FIRST, high for NULLS LAST (Postgres default: ASC sorts NULLS LAST) |
| bool | `0x00` / `0x01` |
| int16/32/64 | Big-endian, sign bit flipped |
| float64 | Negative: invert all bits; otherwise flip the sign bit. One canonical NaN, sorted above all numbers as in Postgres |
| text, bytea | Raw bytes, `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x01` |
| DESC column | Every byte of that column's encoding inverted, terminator included |

- **Collation:** binary (C) collation only.
- **Key size:** fjall limits keys to 64 KiB. The row layer caps encoded index keys well below that (default 8 KiB) and rejects longer values with a clear error.

### Row (value) format

Row values are not order-preserving:

1. Schema version ID
2. Null bitmap
3. Fixed-width columns
4. Variable-length columns

The schema version lets a later `ALTER TABLE` read old rows without rewriting them.

## Implementation Phases

- [ ] **Phase 1: Tuple encoding with property tests and fuzzing** - M1
- [ ] **Phase 2: Row format in the row layer** - M4

## Consequences

### Positive
- Range scans and index order come directly from fjall's byte order.
- Self-delimiting tuples allow composite keys and the version suffix without separators.
- Schema-versioned rows enable cheap schema evolution.

### Negative
- Escaping adds overhead for text and bytea containing `0x00`.
- Binary collation only; locale-aware ordering is not available in the MVP.
- Index keys over 8 KiB are rejected.

## Open Questions

- None specific to this ADR.

## Future Work

- ICU collations: store a sort key in the index and the original value in the row.
- Full Postgres type system (numeric, intervals, etc.).
- On-disk format stability across versions.

## References

- `mvp-fjall-storage-design.md` — Order-preserving tuple encoding; Row and value format
- [ADR-0002: Engine-Owned MVCC and Keyspace Layout](0002-engine-owned-mvcc-and-keyspace-layout.md)
- [ADR-0007: Minimal Catalog and Row Layer](0007-minimal-catalog-and-row-layer.md)
- [ADR-0009: Storage Correctness Testing Strategy](0009-storage-correctness-testing-strategy.md)
