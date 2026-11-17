---
id: 0007
title: Minimal Catalog and Row Layer
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, catalog, indexes]
feature-flag:
created: 2026-11-17
accepted:
implemented:
spec-sections:
  - mvp-fjall-storage-design.md#minimal-catalog-and-row-layer
superseded-by:
---

# ADR-0007: Minimal Catalog and Row Layer

## Status

Proposal

## Summary

A thin `rows` layer over `Txn` maps tables and indexes onto keys. The catalog is three system tables (`sys_tables`, `sys_columns`, `sys_indexes`) stored in the versioned `data` keyspace under reserved table IDs 1–99, so DDL is transactional. Row and index entries are written in the same write set, giving atomic index maintenance; PK, unique and NOT NULL constraints are enforced.

## Context

The MVP must show that tables, primary keys and secondary indexes can be stored and changed transactionally before the relational IR exists. Putting the catalog in the same MVCC keyspace as data ([ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md)) reuses the transaction machinery rather than building a separate metadata store.

## Decision

### System tables

| System table | Columns |
| --- | --- |
| `sys_tables` | table_id, name, schema_version, primary key column list |
| `sys_columns` | table_id, column_id, name, type, nullable |
| `sys_indexes` | index_id, table_id, column list with directions, unique flag |

IDs come from a counter key in `data`, incremented transactionally, so concurrent DDL conflicts and retries like any other transaction.

### Key layouts

| Entry | Key | Value |
| --- | --- | --- |
| Row | `tuple(table_id, 0, pk columns)` | Row format ([ADR-0003](0003-order-preserving-tuple-encoding-and-row-format.md)) |
| Non-unique index | `tuple(table_id, index_id, index columns, pk columns)` | Empty |
| Unique index | `tuple(table_id, index_id, index columns)` | Primary key |

### Behaviour

- **Index maintenance:** `insert`, `update` and `delete` write the row and all its index entries in the same write set, so they commit atomically.
- **Constraints:** primary key, unique and NOT NULL. A uniqueness check is a read of the unique index key, so it enters the read set; two concurrent inserts of the same value conflict at commit under every isolation level (as a write-write conflict on the index key).

## Implementation Phases

- [ ] **Phase 1: Catalog tables, ID counter, transactional DDL** - M4
- [ ] **Phase 2: Row layer, secondary and unique indexes** - M4

## Consequences

### Positive
- DDL is transactional for free.
- Index entries can never diverge from rows.
- Uniqueness holds under concurrency at every isolation level without special locking.

### Negative
- Only a minimal type system and catalog; no `pg_catalog` compatibility.
- The single ID counter key serializes concurrent DDL.

## Open Questions

- None specific to this ADR.

## Future Work

- Foreign keys, CHECK constraints, defaults.
- `ALTER TABLE` beyond adding a nullable column.
- Full Postgres type system and `pg_catalog`.

## References

- `mvp-fjall-storage-design.md` — Minimal catalog and row layer
- [ADR-0002: Engine-Owned MVCC and Keyspace Layout](0002-engine-owned-mvcc-and-keyspace-layout.md)
- [ADR-0003: Order-Preserving Tuple Encoding and Row Format](0003-order-preserving-tuple-encoding-and-row-format.md)
- [ADR-0004: Optimistic Transactions and Isolation Levels](0004-optimistic-transactions-and-isolation-levels.md)
