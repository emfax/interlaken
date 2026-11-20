# Architecture

Interlaken is four library crates:

```text
interlaken-engine   depends on   exec, ir, storage
interlaken-exec     depends on   ir, storage
interlaken-ir       depends on   (none of the others)
interlaken-storage  depends on   (none of the others)
```

| Crate | Owns |
| --- | --- |
| `interlaken-engine` | `Engine` and `Session`, the stored catalog and DDL, privileges, policies and proposals |
| `interlaken-exec` | physical plans, Arrow operators, memory accounting, `EXPLAIN` |
| `interlaken-ir` | types, expressions, operators, the text format, resolver, verifier, optimizer, interpreter, the security core |
| `interlaken-storage` | MVCC transactions on fjall, group commit, version GC, the key encoding, system catalog and rows |

The IR crate doesn't depend on storage: it describes plans and their meaning. The executor depends on both, running IR plans against storage transactions. The engine joins them into a database.

## Storage

`interlaken-storage` is a transactional key-value layer on [fjall](https://github.com/fjall-rs/fjall), an LSM-tree store. Interlaken does its own multi-versioning on top of fjall: each key is stored with the commit timestamp of the version it holds, and a transaction reads the newest versions no later than its snapshot.

- **Transactions** are optimistic. Writes are buffered in the transaction until it commits, and its reads are recorded, so that a single *commit sequencer* can check them against concurrent commits. That check is what provides each isolation level (see [Transactions and isolation](../transactions/isolation.md)).
- **Group commit:** the sequencer writes commits in groups, one write and, under `Durability::Sync`, one sync per group (see [Durability](../operations/durability.md)).
- **Version GC** removes versions no snapshot can see (see [Garbage collection and disk space](../operations/gc.md)).
- **Rows and the system catalog:** a row layer maps tables and indexes onto keys, with an order-preserving key encoding, so range scans follow column order. Index entries are written with their rows, and primary key, unique and NOT NULL constraints are enforced. Table and index definitions live in system tables, so DDL is transactional.

### The async boundary

fjall's API is synchronous and can block on disk. So no async code calls fjall directly. Every read runs on a fixed pool of *storage threads*, `StorageOptions::pool_threads`, behind a bounded queue. Commits run on the commit sequencer's own thread, and GC runs on the storage threads at low priority. Async callers await the result, and a full queue suspends the caller rather than blocking its thread.

The storage API is batched to make that hop cheap: a call reads a batch of keys or a range of rows, never one row, and filters and projections can be pushed into scans so rejected rows never leave the storage thread.

## The IR

`interlaken-ir` defines the typed relational IR that every statement is. A plan is a tree of relational operators (`get`, `filter`, `join`, `aggregate`, `insert` and so on) over scalar expressions, with a fixed type system and Postgres semantics. Plans have two forms:

- **unresolved,** with names, as clients write them in the [IR text language](../ir/syntax.md) or build with the Rust builder;
- **resolved,** with catalog IDs, column bindings and types, pinned to one version of the catalog.

The crate also provides the passes plans go through: the *resolver*, the *verifier*, *security lowering* and the *optimizer* (see [The statement pipeline](pipeline.md)), and a *reference interpreter*. The interpreter is the definition of what a plan means: simple and slow, it is the oracle that the optimizer and executor are tested against.

### The security core

Security lives in the IR crate, as a pass over plans, because every front end lowers into the IR. A policy enforced on the IR applies to every query language alike, and to every operator, subquery and write in a plan.

Policy engines plug in as *providers*: a Cedar provider and an RLS-style provider. A provider partially evaluates its policies for a principal and returns *residuals* in a small security sublanguage. Security lowering turns them into plan operators: a `PolicyFilter` above every read of a governed table, and a `WriteCheck` on every write (see [The security model](../security/model.md)). Both are privileged: only the engine's own passes can create them, and the verifier rejects them in client plans.

## The executor

`interlaken-exec` runs plans. A heuristic physical planner turns an optimized plan into physical operators: scans by primary key range or index, hash and nested-loop joins, index lookup joins, hash aggregation, sorts and top-N, and the rest. They run as a tree of async streams of Arrow record batches: the root pulls, each operator pulls from its inputs as it needs rows, and only the scans at the leaves call storage.

The executor enforces the security barrier at run time. `PolicyFilter` runs before any expression that isn't leakproof sees a row, and the leakproof part of a residual is pushed into storage scans, so rows it rejects never leave the storage thread. `WriteCheck` authorizes each batch of new or changed rows before it is written.

Statements run in transactions with Postgres's semantics: each statement under an implicit savepoint, failed statements undone, and writes invisible to the statement making them, so an update never sees its own output (see [Statements, atomicity and savepoints](../transactions/statements.md)). Blocking operators reserve memory against a per-query budget (see [Memory](../operations/memory.md)).

## The engine

`interlaken-engine` is the public API:

- **`Engine`** opens a database and holds what its sessions share: storage, the catalog cache, the policy provider, the residual cache and the memory pool.
- **`Session`** runs one principal's statements in transactions (see [Sessions and principals](../getting-started/sessions.md)).
- **The catalog** is stored in tables: storage's system tables hold tables and indexes, and the engine's own catalog tables hold columns' IR types, entity types, memberships and function properties (see [Catalog tables](../reference/catalog-tables.md)). DDL methods change both in the caller's transaction.
- **Policies** are versioned in tables too, and change only through proposals that pass verification gates (see [Changing policies](../security/proposals.md)).
- **Privileges** say who may run DDL and change policies (see [Privileges](../security/privileges.md)).

## Further reading

Each layer's design records explain its decisions in depth (see [Design records](../reference/design-records.md)):

| Layer | Start with |
| --- | --- |
| storage | ADR-0001 (fjall and the async boundary), ADR-0002 (MVCC), ADR-0004 (transactions), ADR-0005 (group commit) |
| IR | ADR-0010 (the IR), ADR-0013 (forms, resolver and verifier), ADR-0014 (optimizer) |
| security | ADR-0015 (the security core), ADR-0017 (entities), ADR-0018 (barriers and write checks), ADR-0019 (transactional authorization) |
| executor | ADR-0022 (the executor), ADR-0025 (runtime enforcement), ADR-0026 (statement atomicity), ADR-0027 (memory) |
| engine | ADR-0031 (sessions and the catalog), ADR-0032 (authorization completeness) |
