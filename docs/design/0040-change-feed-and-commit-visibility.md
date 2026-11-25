---
id: 0040
title: Change Feed and Commit Visibility
status: implemented  # proposal | accepted | implemented | superseded
tags: [storage, engine, replication]
feature-flag:
created: 2026-11-25
accepted: 2026-11-25
implemented: 2026-11-25
spec-sections: []
superseded-by:
---

# ADR-0040: Change Feed and Commit Visibility

## Status

Implemented, in four phases. Each phase's *As implemented* notes record where the result differs from this design, chiefly:
- `CommitTs` is storage's `Timestamp`, with a decimal text form;
- `ChangeRetention::Off` makes feeds unavailable rather than live-only;
- subscriptions read rows at the commit's own snapshot, with that moment's policies, rather than with current policies.

## Summary

A program embedding Interlaken can't learn what changed, or when a change has reached the node it is talking to:

- `Session::commit` returns nothing, so the caller never learns its commit's timestamp.
- Nothing waits for `visible_ts` to reach a value. A client that writes on one cluster member and then reads on another can briefly miss its own write.
- Nothing announces commits. The policy prover polls `visible_ts` (ADR-0030), and a program that keeps derived state (routing tables, caches, search indexes, copies in another system) has to poll its tables.

This ADR decides:

1. **Commit timestamps are returned** from every commit as a `CommitTs`, an opaque, ordered value that can be written as a token and parsed back.
2. **Waiting for a timestamp:** `Engine::wait_visible(ts, timeout)`, and `Freshness::AtLeast(ts)` for transactions that must see a given commit.
3. **A change log on every replica**, written in the same batch as the commit it records, kept in memory by default or durably for a configured time.
4. **A change feed** over it: `Engine::changes(filter, from)` streams committed transactions in commit order, per table, with each changed row's key, kind (insert, update or delete) and new values, resumable from a commit timestamp and failing with an explicit gap when that history is gone.
5. **Policy-filtered subscriptions** for sessions follow in a later phase; the first feed is a trusted, embedder-level API.

## Context

### Commit timestamps today

- `Txn::commit` returns `CommitInfo { commit_ts: Option<Timestamp> }`, `None` for a transaction that wrote nothing.
- `interlaken_exec::Transaction::commit` drops it, so `Session::commit` and autocommit statements return `()`.
- `Timestamp` is opaque (ADR-0002): code outside the timestamp source only compares timestamps.
- In a cluster, timestamps are carried in log entries (ADR-0037), so every replica gives a commit the same timestamp. A timestamp from one member is meaningful on every other member of the same cluster.

### Visibility today

- `CommitClock::publish` stores `visible_ts` with a release store and wakes nothing.
- It is called after the single-node sequencer writes a group, after the commit log's apply thread applies an entry, after a replica applies a Raft entry, and after a snapshot install, where `visible_ts` jumps to the snapshot's last commit.
- Replicated databases have a `watch` of the applied *log index*, which fresh reads and forwarded commits wait on. There is no watch of the timestamp, and single-node databases have none at all.
- A commit returns once the local node has applied it, so a session reads its own writes on its own node. Another node may not have applied it yet. `Freshness::Fresh` closes that gap with a read index from the leader, which costs a round trip to the leader on every transaction.

### What a commit records

- A transaction's writes are per key: `WriteOp::Put(value)` or `WriteOp::Delete`. Row and index entries are both plain keys.
- Row keys are `(table_id, 0, pk...)` and index keys `(table_id, index_id, ...)`, so the table, and whether a key is a row, can be read without the catalog. Decoding the primary key and values needs the table's definition.
- Values carry their schema version, and columns can only be added, so a newer definition decodes an older row.
- The commit log and Raft entries hold the same `LogEntry`: consecutive transactions, each a list of `(key, Option<value>)`.
- There is no insert or update distinction below the row layer. The row layer knows: an insert checks that its key is absent, and an update reads the old row.

### What history exists

| Source | Holds | Kept for |
| --- | --- | --- |
| MVCC versions | old values | until the next GC pass past them (10 s by default), or while a transaction pins them |
| `CommitHistory` | keys only, in memory | validation's window; rebuilt empty on open |
| Single-node commit log | full entries | until 4,096 have been applied, and only with `commit_log` set |
| Raft log | full entries | 1,000 to 11,000 entries by default, purged after snapshots |

None of these is suitable to resume a feed from: each is sized for its own job, and two exist only in some configurations.

### Who needs it

- **Programs keeping derived state:** caches, routing tables or configuration rebuilt when the rows behind them change, copies of tables streamed to other systems.
- **Application subscriptions:** clients told when rows they can read change.
- **The policy prover** (backlog `prover-subscriptions`): prover nodes should run as soon as a proposal is applied, instead of polling.
- **Read-your-writes across nodes:** a client carries its last commit's timestamp to whichever node serves its next request.

## Decision

### 1. Commit timestamps

`interlaken_engine::CommitTs` wraps storage's `Timestamp`. It is `Copy`, `Ord` and `Hash`, and has a text form for passing through clients and headers:

```rust
let ts: Option<CommitTs> = session.commit().await?;
let token = ts.map(|t| t.to_string());          // "c1f4a"
let back: CommitTs = token.unwrap().parse()?;
```

- `Session::commit` returns `Option<CommitTs>`: `None` when the transaction wrote nothing.
- `QueryResult` and a finished `QueryStream` report the commit timestamp of an autocommit statement (`commit_ts()`).
- A read-only transaction reports the snapshot it read (`Session::snapshot_ts()`), so a program can read a consistent copy and resume a change feed from exactly that point (section 4).
- The text form is opaque: base-32 of the raw value, stable across versions. Timestamps from different databases are not comparable, and nothing checks that they come from the same one.

### 2. Waiting for a commit

`CommitClock` gains a `tokio::sync::watch` of `visible_ts`, sent from `publish`. Every place that advances `visible_ts` already calls `publish`, so single-node databases, followers and snapshot installs all wake waiters.

- `Engine::visible_ts() -> CommitTs` reads the current value.
- `Engine::wait_visible(ts, timeout)` resolves once `visible_ts >= ts`, or fails with `EngineError::NotVisible` (SQLSTATE `57014`, the statement-timeout code) after `timeout`.
- `TxnOptions::freshness` gains `Freshness::AtLeast(CommitTs)`: the transaction begins once the local replica has applied `ts`, and reads from a snapshot at or after it. This gives read-your-writes across nodes without the leader round trip that `Fresh` costs, because the client says which commit it needs.

A snapshot install moves `visible_ts` past every commit it covers, so a waiter is released correctly when its commit arrives by snapshot.

### 3. The change log

Each replica keeps a **change log**: the writes of each committed transaction, by commit timestamp. It is recorded where `visible_ts` is published, from data already in hand:

- the single-node sequencer has each group's write intents;
- the commit log's apply thread and a replica's apply have the `LogEntry`.

Its retention is a storage option:

```rust
pub enum ChangeRetention {
    /// Keep no change log; the feed only reports commits made after it starts.
    Off,
    /// Keep the most recent changes in memory, up to this many bytes.
    Memory { bytes: u64 },
    /// Also write changes durably, in the same batch as the commit, and keep
    /// them for this long, up to this many bytes.
    Durable { age: Duration, bytes: u64 },
}
```

- **The default is `Memory { bytes: 64 MiB }`.** Values in a decoded `LogEntry` are reference-counted slices, so keeping them costs memory but no copy. History starts empty at every open.
- **`Durable`** writes entries to a `changes` keyspace, keyed by commit timestamp, in the fjall batch that writes the commit. They survive restarts and are exactly as durable as the data. It roughly doubles the bytes written per commit, which ADR-0039's `interlaken_storage_bytes_written_total` shows.
- **Trimming** happens by age and size, in the GC pass. The log's *floor* is the oldest timestamp from which it can replay without a gap.
- **A snapshot install** raises the floor to the snapshot's last commit and drops older entries, since the commits it skipped were never applied here.
- **Index entries are dropped** before recording: the log keeps row keys only, plus the catalog tables' keys (section 4).

#### Insert, update and delete

`WriteOp::Put` gains a flag the row layer sets: `Put { value, inserted: bool }`. The log entry's write tag grows from two values (put, delete) to three (insert, update, delete); older entries' puts read as updates. Only rows written through the row layer are marked. Raw puts are recorded as updates.

### 4. The change feed

```rust
let mut feed = engine.changes(ChangeFilter::tables(["orders", "customers"]), Some(from))?;
while let Some(batch) = feed.next().await {
    let batch = batch?;                    // Err(ChangeError::Gap { .. }) ends the feed
    for change in &batch.changes {         // one committed transaction
        match change.kind {
            ChangeKind::Insert | ChangeKind::Update => apply(change.table(), change.key(), change.row()),
            ChangeKind::Delete => remove(change.table(), change.key()),
        }
    }
    save_position(batch.commit_ts);
}
```

- **One batch per committed transaction**, carrying its `CommitTs` and its changes to the filtered tables. A transaction that touched none of them is skipped.
- **Each change** has the table name and ID, its kind, the primary key as `Datum`s, and the new row (for inserts and updates) decoded with the table's current definition. `ChangeFilter::keys_only()` leaves rows out.
- **Ordering:** batches arrive in commit order, each exactly once in a feed. `from` is exclusive: the feed starts with the first commit after it. A program that saves its position after handling a batch and resumes from it after a crash sees each commit at least once.
- **`from: None`** starts at the current `visible_ts`.
- **Gaps.** If `from` is older than the floor, or a snapshot install skips commits while the feed is open, the feed yields `ChangeError::Gap { from, floor }` and ends. The program resyncs by reading the tables in a read-only transaction, taking its `snapshot_ts()`, and opening a new feed from there.
- **Lag.** A feed reads the change log from its position up to `visible_ts`, then waits on the `visible_ts` watch. A slow consumer falls behind without holding anything up; if trimming passes its position, it gets a gap.
- **Schema changes:** catalog tables are recorded like user tables. When a feed sees a catalog change, it reloads its table definitions. If a filtered table is dropped, the feed yields `ChangeError::TableDropped` and ends. Added columns appear in later rows, and are NULL in older ones.
- **Clusters:** every replica records its own applied commits with the same timestamps, so a program can resume on a different member from the same position, if that member's log reaches back far enough.
- **Security:** `Engine::changes` reads below policies, like `Engine::storage()`. It is for the embedding program's own use, and the documentation says so.

### 5. Subscriptions for sessions (phase 4)

`Session::subscribe(table, from)` serves the same feed to a principal:

- each changed row passes the row policies for `read`, through `PolicyProvider::authorize` with the row as the resource, as point lookups are checked;
- column policies apply as in queries: denied columns are left out, and masks are applied;
- a delete is delivered if the principal could read the row before it was deleted, which needs the old row: the feed reads it from MVCC in a snapshot just before the commit, which the subscription pins while it lags.

Decisions are made with the policies and entities current when each change is delivered, not those at its commit.

### 6. The prover

`watch_proposals` (ADR-0030) subscribes to the proposals table instead of polling `visible_ts`, closing backlog `prover-subscriptions`.

## Alternatives considered

- **Replay from the Raft log.** It already holds every entry, but its retention is set by snapshot policy and it doesn't exist on single-node databases without `commit_log`. Tying feed history to Raft compaction would make one setting serve two purposes.
- **Read history from MVCC versions.** GC removes them within seconds, and keeping them longer slows reads of every key.
- **An in-memory broadcast with no log.** Every slow consumer would either block commits or lose changes, and nothing could resume after a restart.
- **A wait on the client side only** (poll `visible_ts`). Polling adds latency, and every embedder would write the same loop.
- **Insert and update as one `Upsert`.** Cheaper, but consumers that count or audit need the distinction, and the row layer already knows it.

## Testing

- **Timestamps:** commits return increasing timestamps; an empty transaction returns `None`; the text form round-trips; autocommit reports its timestamp.
- **Waiting:** `wait_visible` resolves on single-node and follower, and after a snapshot install; it times out with `57014`; `Freshness::AtLeast` sees the write on another member (cluster test).
- **Feed model test:** random transactions over several tables, including deletes, primary-key updates and rolled-back transactions; the feed's batches, applied in order to an empty copy, must equal the tables at every commit timestamp.
- **Resume:** resuming from every position delivers the remaining commits; a position below the floor fails with a gap; with `Durable`, resuming works across a restart.
- **Gaps:** a follower that installs a snapshot while a feed is open ends the feed with a gap; resyncing from `snapshot_ts()` loses nothing.
- **Schema:** a column added mid-feed; a filtered table dropped.
- **Clusters:** feeds on all three members of a cluster deliver identical batches, including across a leader change (the simulation tests of ADR-0037).
- **Power loss:** with `Durable`, the change log after recovery matches the data (LazyFS tests).
- **Subscriptions:** a principal receives only the rows, columns and deletes it may read.
- **Cost:** ADR-0039's instrumentation-cost test, run with `Memory` and `Durable`, reports the commit throughput each costs.

As implemented, the power-loss case with `Durable`, the simulation tests with feeds, and the cost test aren't built yet: the cluster test checks identical batches across a leader change, and the cost was measured once by hand (phase 2's notes).

## Implementation Phases

### Phase 1: timestamps and waiting

- `CommitTs`, its text form, and its return from `Session::commit`, `QueryResult` and `QueryStream`.
- `Session::snapshot_ts()`.
- The `visible_ts` watch, `Engine::visible_ts`, `Engine::wait_visible` and `Freshness::AtLeast`.

#### As implemented (phase 1)

- **`CommitTs` is an alias** for storage's `Timestamp` (`interlaken_engine::CommitTs`), not a wrapper, so `Freshness::AtLeast` in storage's `TxnOptions` takes it directly. Its text form is the decimal value `Display` already wrote, parsed back by a new `FromStr`; base 32 would have changed `Display` for no gain.
- **The `visible_ts` watch** is a small waiter registry in `CommitClock`, not a `tokio::sync::watch`: the storage crate doesn't depend on Tokio. `publish` stores the new value under the registry's lock and wakes the waiters it satisfies, so a waiter that checked the old value is always woken. `FjallStorage::wait_visible(ts)` and `wait_after(ts)` return its future; `Engine::wait_visible` adds the timeout.
- **`Freshness::AtLeast`** applies to read-only transactions, as `Local` does; read-write ones stay fresh, which also includes every acknowledged commit. `Session::begin` bounds the wait with the session's statement timeout, if one is set, failing with `NotVisible` (`57014`).
- **Snapshots:** `Session::snapshot_ts()` returns the open transaction's snapshot, under READ COMMITTED the current statement's.

### Phase 2: the change log

- `ChangeRetention`, the in-memory log, the `changes` keyspace and trimming.
- Insert and update marked in `WriteOp` and the log entry format.
- Recording from the sequencer, the commit log and replica apply; floors on snapshot install.

#### As implemented (phase 2)

- **Recording:** the change log lives in `CommitClock` beside the waiters. Every path records its entries before publishing their timestamps: the sequencer's group commit, the commit log's apply thread and a replica's apply. Index entries, and keys outside the row layout, are left out when the entry is built.
- **Prior state, not a flag on `Put`:** the write buffer records, from each key's first write in a transaction, whether the key held a row before (`Prior::{Existed, Absent, Unknown}`); the row layer supplies it through `Txn::put_row` and `delete_row`. Later writes keep the first one's answer, so a delete then insert of one key is an update, and an insert then delete is no change. Raw `put` and `delete` record `Unknown`. The commit request and log entry encodings carry it in the write tag's next two bits; entries written before read as `Unknown`.
- **`Off` disables feeds:** with no log, `read_changes` fails with `ChangesUnavailable` (`55000`). A live-only mode would need a buffer anyway, which is what `Memory` is.
- **Durable entries** are written in the same fjall batch as the commit on all three paths, including replaying the commit log on open. Their keys are commit timestamps; values hold the commit time for age-based trimming. The floor is stored in `meta` (`changes_floor`), and the index of entry sizes and times is rebuilt from the keyspace on open. An open that doesn't keep a durable log removes one a previous open left, so a later durable open can't see a hole in it. The memory part also caches recent durable entries, up to 64 MiB.
- **Snapshot installs** remove durable entries and set the floor in the batch that switches generations, then reset the memory part before publishing.
- **Cost**, measured on 32 concurrent writers of one 100-byte row each with buffered durability, three rounds: the default `Memory` log is within noise of `Off` (about 2% either way); `Durable` costs about 25% of commit throughput. The instrumentation-cost test doesn't cover it yet.

### Phase 3: the feed

- `Engine::changes`, `ChangeFilter`, decoding with the catalog, gaps and dropped tables.
- The prover on the feed.
- The book: a chapter on change feeds and read-your-writes.

#### As implemented (phase 3)

- **`Engine::changes` is async,** since it resolves table names in a read-only transaction. A feed reads `PAGE` (256) entries at a time and waits with `wait_after` when it has caught up.
- **Decoding** uses storage's catalog (`catalog::tables`), not the engine's, since a change needs only the stored definition. Writes to storage's system tables (IDs below 100) mark a catalog change: the feed forgets its definitions and checks that each filtered name still has the ID it had, ending with `TableDropped` (`42P01`) otherwise. A system table's writes sort first in an entry, so a column added in the same transaction is known before the rows that use it are decoded.
- **`ChangeFilter::all()`** leaves out tables named `engine_` or `sys_`; naming one explicitly follows it, which is how the prover follows `engine_policy_proposals`.
- **The prover** follows that table with `keys_only`, proving once when its feed opens and after each batch. A gap reopens the feed; with no change log it falls back to checking `visible_ts` every `poll`.
- **Errors end the feed:** after one, `next()` returns `None`.

### Phase 4: session subscriptions

- `Session::subscribe` with privileges, row and column policies, and deletes checked against the old row.

#### As implemented (phase 4)

- **Two reads per commit instead of `authorize`:** for each batch, the subscription reads the changed keys through the session's own secured query, once at the snapshot just before the commit (`FjallStorage::begin_before`) and once at the commit (`begin_at`), and compares: visible after and not before is an insert, both an update, before only a delete. Row policies, column policies, masks and entity paths all apply exactly as in queries, and a principal sees rows arrive and leave as their access changes, not only as they're inserted and deleted. `PolicyProvider::authorize` isn't used on the engine's read path, and would have needed every entity a policy reaches loaded up front.
- **Policies of the commit, not current ones:** each read uses the snapshot's own policies and entities, which is more consistent than the design's current policies, and makes a policy change apply to later commits only.
- **Old rows come from MVCC,** kept by a `HistoryPin` at the subscription's position: a pin that doesn't expire, moved up after each batch. The clock's *GC floor* is the newest watermark any GC pass used; a snapshot older than it can't be pinned, so `subscribe` fails with `55000` if `from` is older. A pin holds back GC only for versions newer than it, so an idle subscription holds nothing back.
- **No separate privilege:** what a principal can read is decided by policies alone, as for queries. A denied column without a mask fails the subscription with `42501`, as a query reading it would.

## Consequences

### Positive

- Read-your-writes across a cluster costs a wait on the local replica, not a leader round trip.
- Programs stop polling their tables; derived state updates within one apply of the commit.
- The same feed serves internal watchers, embedders and, later, application subscriptions.
- `Durable` retention makes feeds resumable across restarts and node moves.

### Negative

- `Session::commit`'s return type changes, which breaks every caller that matches on `()`.
- The default in-memory log costs up to 64 MiB per node.
- `Durable` roughly doubles bytes written per commit.
- Feeds deliver changes decoded with today's table definitions, not those at the commit.
- Session subscriptions decide with current policies, so a policy change applies to changes committed before it.

## Open Questions

- Should `Durable` share the commit's batch for every commit, or be written by a background task with its own durability, trading a window of loss for write cost?
- Should a feed offer old rows for updates as well as deletes, at the cost of pinning MVCC history for as long as the feed lags?
- Should feeds filter by key range or predicate, or is filtering by table enough?

## Future Work

- Old values for every change (*before images*), for consumers that compute differences.
- A feed of DDL as statements rather than catalog rows.
- Archiving the change log to object storage, which point-in-time restore could share.

## References

- [ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md): timestamps and the keyspace layout.
- [ADR-0005](0005-commit-sequencer-group-commit-and-durability.md): the sequencer.
- [ADR-0030](0030-policy-proposals-and-distributed-verification.md): the prover's polling.
- [ADR-0037](0037-replication-with-raft.md): the commit log, replica apply and freshness.
- [ADR-0038](0038-streaming-snapshots.md): snapshot installs.
- [ADR-0039](0039-telemetry-and-cost-attribution.md): bytes-written metrics.
- Backlog `prover-subscriptions`.
