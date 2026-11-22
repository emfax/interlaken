---
id: 0038
title: Streaming Snapshots
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, replication, network]
feature-flag:
created: 2026-11-23
accepted:
implemented:
spec-sections: []
superseded-by:
---

# ADR-0038: Streaming Snapshots

## Status

Proposal.

## Summary

[ADR-0037](0037-replication-with-raft.md) replicates the database with Raft. A replica that is new, or too far behind for the log to catch it up, receives a *snapshot*: a full copy of the data. Today every replica builds one every 10,000 log entries and stores it whole, even if no replica ever needs it. Snapshots are held in memory, sent as one message, and copy every stored version, not just current data. Memory, disk writes and network traffic all grow with the database's stored history, not with what changed or who needs it.

This ADR changes four things:

1. **Snapshots are made on demand.** A snapshot is exported from a consistent view of the current data when a replica needs one. Periodic snapshot building only records how far the log can be compacted. Nothing is copied or stored.
2. **Snapshots hold current data only:** the newest version of each key at the snapshot's timestamp, without deleted keys.
3. **Snapshots stream** in chunks of about 1 MiB, with flow control, so neither side holds a whole snapshot in memory.
4. **A replica installs a snapshot into a new data keyspace** and switches to it atomically. It writes the data once, readers never see it incomplete, and a crash leaves the old state intact.

## Context

### How snapshots work today (ADR-0037 phase 4)

- **Building.** openraft calls the state machine's snapshot builder every `snapshot_every` entries (10,000 by default).
  1. The builder captures a consistent fjall view between applies.
  2. Storage exports the whole `data` keyspace from it, every stored version included, plus the last commit timestamp, the GC watermark, and Raft's applied log ID and membership.
  3. The export goes into one `Vec<u8>`.
  4. The blob is stored in the `replica_snapshot` keyspace, in 1 MiB chunks under a new generation, which a synced `meta` pointer makes current.
  5. openraft then purges the log up to the snapshot, keeping the last `logs_kept` (1,000) entries, after storage syncs the applied data.
- **Sending.** When a follower needs purged entries, openraft fetches the stored snapshot and the network sends it as one message: one gRPC call of up to 1 GiB.
- **Installing.** The follower stores the whole blob, then imports it:
  1. a marker in `meta` is synced;
  2. `data` is cleared and refilled;
  3. the snapshot's `meta` entries are written, and the marker removed;
  4. everything is synced.

  A restart that finds the marker redoes the import from the stored copy. Transactions reading during an import can see `data` incomplete.

### What that costs

For a database holding *D* bytes of stored versions, of which *L* bytes are live data:

| | Today | Proportional to |
| --- | --- | --- |
| Disk written per 10,000 entries, on every replica | *D* (the stored copy) | database size, whether or not anyone needs a snapshot |
| Disk held on every replica | *D* extra (the stored copy) | database size |
| Memory to build, send or install | *D* | database size |
| Network per snapshot sent | *D* | stored history |
| Disk written on an installing replica | 2*D* (stored copy, then the import) | stored history |

*D* exceeds *L* by every old version and deletion marker that garbage collection hasn't removed yet. On a table with frequent updates, that can be most of the keyspace.

### Why most of that isn't needed

- **The replica's data already is its state.** Unlike a state machine kept in memory, a replica's `data` keyspace is durable and current. Any consistent view of it, together with the applied log ID read from the same view, is a valid snapshot covering everything applied. A stored copy adds nothing that can't be rebuilt at any moment.
- **Old versions are useless on the receiver.** After an install, the receiver's validation history restarts at the snapshot's last commit, so its transactions with older snapshots fail anyway (ADR-0037 phase 4). Only the newest version of each key at or below the snapshot's timestamp can ever be read there.
- **Other systems build snapshots on demand.** CockroachDB builds a range's snapshot from the storage engine when it sends it, rather than keeping one. etcd and TiKV stream snapshots in chunks.

### What doesn't change

Raft still needs snapshots, for new and far-behind replicas. The log is still purged only past entries whose applied data is synced. Snapshot sending stays inside `interlaken-replication` and its `Transport`, and snapshots remain logical exports: fjall 3.1 has no checkpoint API, which would allow physical copies by hard-linking table files.

## Decision

### 1. Snapshots on demand

- **Building records a position, not a copy.** The snapshot builder syncs the applied data and returns the snapshot's metadata: the applied log ID and membership at that moment. That is all openraft needs to purge the log up to it. Nothing is exported or stored. The `replica_snapshot` keyspace is removed.
- **The current snapshot is always fresh.** `get_current_snapshot` runs on the state machine's task, between applies. It captures a fjall view, reads the applied log ID and membership from that same view, and returns them with a handle on the view as the snapshot's data. The metadata matches the data exactly, so:
  - a snapshot never claims more than it holds;
  - openraft's restart check, which installs the current snapshot only if it is newer than the applied state, never fires, since the two are equal.
- **The view is exported when sent.** The network streams the export straight from the view (section 3). Holding the view pins fjall's versions for the length of the transfer, which delays fjall's own compaction, not Interlaken's GC.
- **Purging stays safe.** The builder syncs before reporting the position, and purge already syncs again. Every purged entry is covered by synced data, which any later view includes.

### 2. Current data only

- **The export holds the newest version of each key** whose timestamp is at or below the snapshot's last commit, with its commit timestamp. A key whose newest such version is a deletion marker is left out. Index entries and catalog rows are keys in `data` like any other.
- **The receiver writes each version at its original timestamp,** so reads and validation behave as if the history up to then were present.
- **Older snapshots on the receiver can't be served.** Installing expires every pin older than the snapshot's last commit, so those transactions fail with `25P04` as an expired transaction does, and the receiver's GC watermark becomes the snapshot's last commit.
- **Encoding (version 2):**
  - a header: magic, last commit timestamp, the `meta` entries named by the caller;
  - then entries `(key, commit_ts, value)`, length-prefixed.

  Entries follow storage order, so the receiver writes them in key order.

### 3. Streaming

- **Chunks.** An export is a sequence of chunks of about 1 MiB of entries. The first chunk carries the header and the Raft metadata; the last says so.
- **The `Transport` gains a snapshot stream.** `Request::Snapshot` becomes `Request::SnapshotChunk { id, seq, last, bytes }`.
  - `id` names one transfer: the sender's node and a per-transfer counter.
  - `seq` orders the chunks.
  - The receiver acknowledges each chunk.
  - The in-memory and turmoil transports send chunks as individual requests.
- **Over gRPC, a client-streaming method.** `interlaken-net` adds `/interlaken.Replica/Snapshot`, so HTTP/2 flow control gives backpressure, and the 1 GiB message limit returns to the default.
- **openraft's `full_snapshot`** drives the stream. It honours the cancel signal between chunks and maps transport errors to `StreamingError`. A failed transfer is retried from the start, with a new view: a snapshot is cheap to begin again, since nothing was stored.
- **Memory.** The sender holds one chunk at a time, the receiver one batch of writes. Neither grows with the database.

### 4. Installing into a new keyspace, switched atomically

- **Data keyspaces have generations.** `data` becomes `data.<generation>`, and a `meta` entry names the current generation.
  - The engine resolves it on open.
  - After an install, the engine swaps the current keyspace behind a shared handle that readers, writers, apply and GC load on each use.
- **Receiving writes the new generation once.** Chunks are written as they arrive into `data.<n+1>`, unsynced, in batches.
- **The switch.** When the last chunk has arrived, the receiver:
  1. syncs the new keyspace;
  2. writes, in one synced batch, the new generation pointer, the snapshot's last commit timestamp and GC watermark, and its applied log ID and membership;
  3. swaps the handle;
  4. publishes the new visible timestamp and restarts validation history;
  5. expires older pins;
  6. deletes the old keyspace.
- **Crashes.** Before the pointer is synced, a restart finds the old generation current and deletes the partial new one. The replica is exactly as it was, and the leader sends a snapshot again. After the pointer is synced, the new state is complete. No stored copy or redo marker is needed.
- **Readers.** Transactions already reading keep the old keyspace until they finish; ones that began before the snapshot's timestamp then fail when they next touch storage (section 2). No transaction ever sees a half-installed snapshot.

### What each change saves

| | Today | After this ADR |
| --- | --- | --- |
| Disk written per 10,000 entries, every replica | *D* | none (a sync and a purge) |
| Disk held for snapshots, every replica | *D* | none; a sender's open view pins fjall versions during a transfer |
| Memory to build, send or install | *D* | about one chunk |
| Network per snapshot | *D* | *L* |
| Disk written by an installing replica | 2*D* | *L* |
| Disk held by an installing replica during the transfer | *D* (stored copy) beside its data | *L* (the new generation) beside its old data, freed at the switch |

## Testing

- **Unit:**
  - the version-2 export keeps exactly the newest version at or below the snapshot timestamp, drops deleted keys, and round-trips;
  - damaged chunks are refused before anything is switched.
- **Storage:** installing a generation and switching to it.
  - Transactions open at the switch keep their view, and ones older than the snapshot fail with `25P04`.
  - A crash at every step, simulated by stopping before each one and reopening, leaves either the old state or the new, never a mixture.
  - The old keyspace is deleted.
- **Memory bound:** a snapshot several times larger than a memory cap, built, sent and installed. The workspace's counting allocator (`interlaken-test-alloc`) checks peak memory stays within a few chunks.
- **Writes saved:** with snapshots on demand, a replica committing past many `snapshot_every` thresholds writes nothing for snapshots. Disk-space and bytes-written counters check this.
- **openraft's storage suite** passes with snapshots built on demand.
- **Clusters:** the existing membership tests (a new replica built from a snapshot, a far-behind replica catching up) pass unchanged.
  - A transfer cut off midway, by a partition or by killing either end, is retried and completes.
  - The turmoil simulation uses data large enough that snapshots span many chunks, under the same faults.
  - The LazyFS power-loss test of ADR-0037 cuts power during installs.

## Implementation Phases

- [ ] **Phase 1: Current data only.**
  - Export version 2, with the newest version per key and no deleted keys.
  - Pin expiry and the GC watermark on install.
  - *Testing:* unit and storage tests; the cluster tests unchanged.
- [ ] **Phase 2: Snapshots on demand.**
  - The builder records a position.
  - `get_current_snapshot` captures a view.
  - The `replica_snapshot` keyspace is removed.
  - *Testing:* openraft's suite; the writes-saved test.
- [ ] **Phase 3: Streaming.**
  - Chunked export.
  - `SnapshotChunk` in the `Transport`, the in-memory and turmoil transports, and the gRPC streaming method.
  - *Testing:* the memory bound; interrupted transfers; the simulation with large snapshots.
- [ ] **Phase 4: Install by generation.**
  - Data keyspace generations and the swappable handle.
  - Receiving into a new generation and the atomic switch.
  - Recovery on open.
  - *Testing:* the crash-at-every-step tests; power loss during installs.

Phase 3 can begin before phase 4 by streaming into the current import. Phase 4 then removes the remaining stored copy.

## Consequences

### Positive

- Replicas stop writing and storing a full copy of the database every 10,000 entries.
- A snapshot's size follows live data, not stored history, and memory stays bounded whatever the database's size.
- Installs write the data once, never expose a half-installed state to readers, and need no redo machinery.
- The 1 GiB message limit goes away.

### Negative

- Data keyspace generations touch every place that uses `data`: reads, writes, apply, GC, export.
- A transfer holds a fjall view open, delaying fjall's compaction of the versions it pins. A very slow transfer to a far-away replica holds it longer.
- A transfer that fails restarts from the beginning; resuming would need the same view kept across attempts.
- During an install the receiver holds its old data and the new generation at once, briefly doubling its data on disk.
- Transactions on the receiver older than the snapshot fail. They fail today too, at validation; with this ADR their reads fail as well.

## Open Questions

- **Is a fixed 1 MiB chunk right,** or should chunks follow HTTP/2's window, or adapt to the link?
- **Should a far-behind replica try the log first?** It might fall back to a snapshot only if the entries it needs are gone. openraft decides that today, from `logs_kept`.
- **Could physical copies follow?** If fjall gains checkpoints, could snapshots of very large databases become hard-linked table files streamed as-is, avoiding the logical export? The receiver would still need only current versions, which a physical copy doesn't give.

## Future Work

- **Resuming an interrupted transfer** from its last acknowledged chunk, by keeping the sender's view for a while.
- **Snapshots from a nearby follower** rather than the leader, to spare the leader's network.
- **Compressing chunks** on slow links.

## References

- [ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md): MVCC and the keyspace layout
- [ADR-0008](0008-version-garbage-collection.md): version garbage collection and pin expiry
- [ADR-0037](0037-replication-with-raft.md): replication with Raft, phases 4 and 5
- openraft: `RaftStateMachine::get_current_snapshot`, `RaftNetworkV2::full_snapshot`
