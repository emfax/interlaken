---
id: 0044
title: File System
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, engine, security, replication]
feature-flag:
created: 2026-11-25
accepted:
implemented:
spec-sections: []
superseded-by:
---

# ADR-0044: File System

## Status

Proposal. Builds on ADR-0040 (change feed) and ADR-0042 (namespaces).

## Summary

Applications built on Interlaken keep files too: uploads, assets, compiled components, documents. Today they must either store them in a `bytea` column, limited by the 64 MiB write set and with every read loading the whole value, or keep them outside the database, where they aren't replicated, transactional or governed by the same policies as rows.

This ADR adds a file system inside the database:

1. **Files and directories are rows** in system tables per namespace, with paths, sizes, hashes and timestamps, so their metadata commits in ordinary transactions, replicates and appears in the change feed.
2. **Content is stored in chunks** of up to 1 MiB, addressed by their BLAKE3 hash, written and read as streams. A file can be far larger than a transaction's write set.
3. **Chunks are stored in the database by default,** so Raft replicates them and snapshots stream them, or **in object storage** for a namespace that sets it, with only metadata replicated.
4. **Policies govern files** like rows: files and directories are entity types, and a file is a member of its directories, so `resource in Dir::"..."` works.
5. **Quotas** per namespace limit bytes and file count.
6. **An async API** with POSIX-like operations: create, open, read, write, stat, list, rename, remove.

## Context

### What storage offers

- **Write sets** are limited to 64 MiB of keys and values per transaction (`max_write_set_bytes`). gRPC messages are limited to 64 MiB, and a commit group is one Raft entry, so one transaction can't carry a large file.
- **Values** can be large (`u32` lengths in the row format), but nothing streams them: a `bytea` column is read and written whole, and counts against the executor's memory budget.
- **fjall** runs without key-value separation, so large values are rewritten by every compaction.
- **Snapshots** stream the `data` keyspace in chunks of about 1 MiB, each holding at least one whole key and value (ADR-0038). Values of about 1 MiB fit that well.
- **MVCC** keeps old versions until GC passes them.

### What policies offer

- Entity types are backed by tables. Their attributes are columns, and references are columns holding another entity's key (ADR-0017).
- Memberships keep a closure table, so `in` tests are one indexed lookup however deep the hierarchy. Directory trees are exactly that shape.
- Writes are checked with `PolicyProvider::authorize` against the row as written (ADR-0018).

### Requirements

- POSIX-like operations through an async API, governed by the same principals and policies as rows.
- Chunked, streaming writes and reads.
- File metadata that can commit inside a database transaction.
- BLAKE3 hashes for each chunk and each whole file.
- Quotas per namespace.
- Content replicated by Raft, or optionally kept in object storage.
- Snapshots that carry file content.
- Change-feed entries for metadata.

## Decision

### 1. Metadata as rows

Each namespace that uses files has two system tables, created by `enable_files(namespace, FileOptions)`:

| Table | Key | Columns |
| --- | --- | --- |
| `ns.fs_nodes` | `id` (text, generated) | `parent` (id, null for the root), `name`, `kind` (`dir` or `file`), `owner` (principal text), `size`, `hash` (BLAKE3 of the content), `chunks` (array of chunk hashes), `created`, `modified` (commit timestamps), `attributes` (`jsonb`) |
| `ns.fs_tree` | (`descendant`, `ancestor`) | `paths`: the closure table of `fs_nodes` under its parents |

- A unique index on (`parent`, `name`) keeps names unique within a directory.
- Paths are resolved by walking names from the root, one indexed lookup per level.
- **Entity types:** `File` and `Dir` are declared over `fs_nodes` (filtered by `kind`), with `owner`, `name`, `size` and `attributes` as attributes, and a membership of nodes in their parent directories kept in `fs_tree`.
- **The tables are governed:** applications don't write them directly; the file API does, through the same statement path as other writes. They can be read with ordinary queries, which policies filter.
- **The change feed** (ADR-0040) reports changes to them like any table.

### 2. Content in chunks

- **Chunks** are up to 1 MiB, stored by their BLAKE3 hash in `ns.fs_chunks(hash, bytes)`. Equal chunks are stored once per namespace.
- **The file hash** is the BLAKE3 hash of the whole content, computed while writing and recorded in `fs_nodes.hash`. Readers verify each chunk against its hash as it arrives.
- **Writes are staged.** A `FileWriter` commits each full chunk in a short transaction of its own, as an unreferenced chunk. When the writer finishes, the file's metadata row (with its list of chunk hashes) is written in the caller's transaction. So:
  - a file of any size is written in bounded memory and bounded transactions;
  - the file appears, changes or disappears atomically with whatever else that transaction does;
  - a writer that never finishes leaves unreferenced chunks, which are collected (section 6).
- **Overwrites** write a new chunk list. Chunks the old and new versions share are reused; a file is never modified in place.
- **Reads** use the caller's snapshot: a `FileReader` reads the metadata row, then streams chunks, which the snapshot pins. Reads can seek to any offset, starting at the chunk that holds it.

### 3. Where chunks live

`FileOptions::store` picks it per namespace:

- **`ChunkStore::Database`** (the default): `fs_chunks` is a table in `data`. Raft replicates chunks with everything else, and snapshots carry them (ADR-0038), each chunk one value. The `data` keyspace enables fjall's key-value separation for values over 64 KiB, so compactions don't rewrite chunks.
- **`ChunkStore::Object(config)`:** chunks go to an object store (S3-compatible, through the `object_store` crate) under `namespace/hash`, uploaded before the metadata commits. Only metadata is replicated, and snapshots carry references, not content. Object storage keeps content safe; the cluster keeps it consistent.

Changing a namespace's store copies chunks in the background, then switches.

### 4. Policies

File operations map onto the four actions:

| Operation | Action and resource |
| --- | --- |
| `stat`, `open` for reading, `read_dir` | `read` on the node |
| `create`, `create_dir` | `insert` on the new node, as it would be written |
| writing, `set_attributes` | `update` on the node |
| `rename` | `update` on the node, with its new parent and name |
| `remove` | `delete` on the node |

A policy like

```cedar
permit (principal, action == Action::"read", resource is File)
  when { resource in Dir::"public" || resource.owner == principal };
```

governs files exactly as it would rows. Directory listings and queries over `fs_nodes` are filtered by the same policies.

### 5. The API

```rust
let fs = session.files("app")?;                       // a namespace's files
fs.create_dir("/uploads").await?;
let mut w = fs.create("/uploads/report.pdf").await?;  // FileWriter: AsyncWrite
tokio::io::copy(&mut body, &mut w).await?;
let meta = w.finish().await?;                         // metadata in the session's transaction
let mut r = fs.open("/uploads/report.pdf").await?;    // FileReader: AsyncRead + AsyncSeek
let entries = fs.read_dir("/uploads").await?;
fs.rename("/uploads/report.pdf", "/archive/report.pdf").await?;
fs.remove("/archive/report.pdf").await?;
```

- **POSIX-like, not POSIX.**
  - There are directories, files, names and metadata.
  - There are no hard links, permission bits (policies take their place), sparse files or in-place partial writes.
  - Symbolic links are left for later.
- **Errors** have SQLSTATEs: a missing path is `58P01`, an existing one `58P02`, a non-empty directory `2BP01`, a quota `53400`.
- **Transactions:** every operation runs in the session's transaction, or an implicit one, like a statement. Concurrent changes to the same node conflict as row writes do (`40001`).

### 6. Quotas and collection

- **Quotas:** `FileOptions::quota { bytes, files }`. Usage is kept in one row per namespace, updated in each writing transaction, so it's exact. A write that would exceed the quota fails with `53400`. Staged chunks count from when they're written, so a writer can't exceed the quota before finishing.
- **Collection:** chunks no node refers to are deleted once they're older than `FileOptions::staging_grace` (default one hour), by a pass beside version GC. Writers in progress keep their chunks alive by touching them.

### 7. Namespaces

Files belong to a namespace (ADR-0042): its owner owns its root, its quota applies, and dropping it with cascade removes its files. Until ADR-0042 is built, files live in the one default namespace.

## Alternatives considered

- **Files as `bytea` values.** Simplest, but limited by the write set and memory budget, and every read loads the whole file.
- **Content outside the database only** (always object storage). It leaves embedded and single-node deployments without files, and makes object storage a dependency of every cluster.
- **A separate, non-transactional blob keyspace** replicated outside the commit path. It would avoid MVCC for content, but need its own replication, snapshots and garbage collection; chunks as rows reuse all three.
- **Chunks written in the caller's transaction.** Atomic, but it limits files to the write set.
- **Content-defined chunking** (variable chunk boundaries). It deduplicates edited files better, but fixed 1 MiB chunks make seeking trivial and fit snapshot chunks.

## Testing

- **A model test:** random sequences of create, write, overwrite, rename, remove and read across concurrent sessions, compared with an in-memory file tree; aborted transactions leave no trace except collectable chunks.
- **Large files:** a file of several GiB written and read in bounded memory (the memory-accounting allocator of the streaming tests).
- **Hashes:** corrupted chunks are detected on read; equal content gives equal file hashes.
- **Policies:** the security model tests with files: directory policies, owners, listings filtered.
- **Quotas:** concurrent writers can't exceed a quota together.
- **Replication:** files written on followers, read on every member; a snapshot install carries content; simulation tests with files.
- **Object storage:** against a local S3-compatible server in CI; an upload that fails leaves no metadata.
- **Collection:** unreferenced chunks go after the grace period, and referenced ones never do.

## Implementation Phases

### Phase 1: metadata and chunks in the database

- `enable_files`, the system tables, entity types and the tree membership.
- `FileWriter` and `FileReader`, staging, hashes, and the operations of section 5.
- Policies on every operation.

### Phase 2: quotas and collection

- Usage rows, quota errors, the collection pass.
- Key-value separation for large values in `data`.

### Phase 3: object storage

- `ChunkStore::Object`, uploads before commit, references in snapshots, moving a namespace between stores.

### Phase 4: the book

- A chapter on files: the API, policies over files, quotas and stores.

## Consequences

### Positive

- Files are transactional with rows, replicated, in snapshots, in the change feed, and governed by the same policies.
- File size isn't limited by the write set or memory budget.
- Deduplication within a namespace comes free with content addressing.

### Negative

- Every file's content passes through Raft in the default store, which adds replication traffic and disk writes in proportion to what's stored.
- One usage row per namespace makes concurrent writers in a namespace conflict on it.
- Staging means a crashed writer's chunks occupy space until the grace period ends.
- Key-value separation changes the `data` keyspace's on-disk format and compaction behaviour.

## Open Questions

- Should quota usage be kept in several rows per namespace, summed when checked, to reduce conflicts between concurrent writers?
- Should the change feed carry chunk lists or only metadata?
- Should reads from object storage be cached on each node, and how is that cache bounded?

## Future Work

- Symbolic links.
- Range writes that replace only the chunks they touch.
- Serving files to WASM components through `wasi:filesystem`, once WASM functions exist (ADR-0029).

## References

- [ADR-0017](0017-entities-and-hierarchy-closure-table.md): entities and closure tables.
- [ADR-0018](0018-policy-barrier-and-write-check-plan-constructs.md): write checks.
- [ADR-0038](0038-streaming-snapshots.md): streaming snapshots.
- [ADR-0040](0040-change-feed-and-commit-visibility.md): the change feed.
- [ADR-0042](0042-namespaces-and-scoped-policies.md): namespaces.
