---
id: 0037
title: Replication with Raft
status: proposal  # proposal | accepted | implemented | superseded
tags: [storage, replication, network, security]
feature-flag:
created: 2026-11-22
accepted:
implemented:
spec-sections: ["mvp-fjall-storage-design.md#hooks-for-later-layers", "mvp-completion.md#beyond-the-mvp"]
superseded-by:
---

# ADR-0037: Replication with Raft

## Status

Proposal. Phase 1, the commit log on one node, is built: it changes no behaviour, and readies the commit path for the rest. Phase 2, replication in process, is built: `interlaken-replication` replicates storage with openraft, tested in simulated clusters. Phase 3, reads, is built: fresh snapshots are linearizable, and local ones are available for read-only transactions.

## Summary

Replicate the whole database with one Raft group: the shape the storage ADRs prepared for, and that ADR-0005 calls the Raft hook.

- **The commit path is split in two.**
  - *Proposing*, on the leader: it validates transactions as the sequencer does today, and proposes each validated group as one log entry.
  - *Applying*, on every replica: it writes the entry's batch to fjall.
- **Commit timestamps come from the log position**, so every replica holds identical versions (ADR-0002).
- **Transactions can run on any node.**
  - Reads use the local replica.
  - Commits go to the leader.
  - A transaction's snapshot is fresh by default: a read index from the leader.
  - A commit returns once the local node has applied it, so a session reads its own writes.
- **Version GC, statistics and caches stay per replica.**
- **Implementation choices:**
  - Replication is a new crate, `interlaken-replication`, behind a `CommitPath` trait that storage exposes. The single-node sequencer stays as the other implementation, unchanged for embedded use.
  - Raft itself comes from openraft, behind Interlaken's own traits.
  - The network is a narrow transport trait of Interlaken's own, implemented in memory for deterministic simulation tests and with gRPC over mutual TLS (tonic, which is built on tower) for production.

## Context

### What the storage layer prepared

- **One commit point.** The commit sequencer (ADR-0005) is a single thread. It validates each transaction's read set, dependencies and write intents against an in-memory history of recent commits (ADR-0004), assigns consecutive commit timestamps, writes each group as one fjall batch and only then advances `visible_ts`. ADR-0005: *"The sequencer becomes the leader's proposal path; an apply thread writes the same fjall batches; `commit_ts` derives from the log."* ADR-0004: *"the rest of the layer stays the same."*
- **Engine-owned MVCC.** Versions are `key ++ commit_ts` in the `data` keyspace, so *"versions remain meaningful across replicas when Raft applies the same log"* (ADR-0002). A `raftlog` keyspace is planned beside `data` and `meta`, and `commit_ts` is a `u64` a log-derived value fits.
- **An unknown outcome is already an error.** If writing a group fails, the sequencer answers `CommitOutcomeUnknown`. A leader that loses office with entries in flight is the same situation. The error has no SQLSTATE of its own yet, so it reaches clients as `XX000`.
- **The layers above expect replication:**
  - per-node residual caches invalidated by replicated versions (ADR-0019);
  - policy provers that will watch replicated proposals (ADR-0030 phase 2, backlog `prover-subscriptions`);
  - cooperative yielding in the executor partly to protect heartbeats (ADR-0022).

### What doesn't exist yet

- **No network:** the engine is embedded, with no server, wire protocol or inter-node transport. pgwire is future work.
- **No reads across nodes:** snapshots, `visible_ts` and the validation history are local.
- **No membership, snapshots for new replicas, or log compaction.**

### Requirements

- **Isolation holds across the cluster.** Serializable stays serializable, and the weaker levels (ADR-0004) keep their meaning.
- **Durability:** an acknowledged commit survives the loss of a minority of nodes, and power loss on any of them.
- **Single-node embedded use doesn't pay for replication.**
- **Testability.** The storage layer's correctness rests on model, crash, power-loss and Elle tests (ADR-0009). Replication needs at least the same, with partitions, delays and leader changes, run deterministically.

## Decision

### Shape: one Raft group, leader-validated

The whole database is one Raft group, typically of three or five voters. Sharding by key range, timestamp oracles and cross-range transactions stay future work (ADR-0001).

### The commit path

Storage gains a `CommitPath` trait: submit a `CommitRequest` (snapshot, reads, dependencies, write intents), get back a commit timestamp or an error. There are two implementations.

- **`LocalSequencer`** is today's sequencer, unchanged, for embedded single-node use.
- **`RaftCommit`**, in `interlaken-replication`, is used when the engine runs as a cluster member.
  1. **Forwarding.** A transaction on a follower sends its `CommitRequest` to the leader. The leader takes requests from its own node and from followers alike.
  2. **Validation.** The leader validates as the sequencer does: against the history, and against earlier members of the group. Survivors form a group.
  3. **Proposal.** The group's write intents, with their values, become one log entry. The leader keeps validating new requests against entries proposed but not yet committed, which are ordered before them. Validation and replication overlap, which is what backlog `pipelined-commit-validation` wanted for fsync.
  4. **Commit.** Once a majority has persisted the entry (each fsyncs its log under `Durability::Sync`), the entry is committed.
  5. **Apply.** Every replica, leader included, applies committed entries in order on one apply thread. It writes the batch to fjall with the entry's timestamps, adds the writes to its validation history and advances `visible_ts`.
  6. **Reply.** The node where the transaction ran replies once *it* has applied the entry, so the session's next transaction there sees the commit.
- **Leader changes.** A leader that loses office answers its in-flight requests with `CommitOutcomeUnknown`, since the new leader may still commit the entry, and forwarders pass that on. Every replica builds the validation history as it applies, so a new leader validates from its own applied state plus its log's uncommitted tail.
- **What isn't replicated:** the fjall data writes themselves are reproducible from the log, so applying uses buffered persistence. The log is the durable record, and the data keyspace is synced before the log is compacted past it.

### Commit timestamps

- **Formula:** `commit_ts = base + index × 1024 + position`, where `index` is the entry's log index and `position` is the transaction's place in its group. `max_commit_group` is capped at 1,024.
- **Every replica computes the same timestamps** from the log alone, and they rise with log order.
- **Gaps are harmless:** visibility needs order, not density (ADR-0004).
- **`base`** is the last commit timestamp of the database the cluster was created from, plus one. An existing single-node database can then become a cluster's first member without rewriting its versions.

### Reads and snapshots

Reads always come from the local replica's fjall. What varies is which snapshot a transaction starts at:

| `TxnOptions::freshness` | Snapshot | Cost |
| --- | --- | --- |
| `Fresh` (default) | the leader's commit index, obtained by Raft's read-index protocol, waited for locally | one round trip to the leader, batched across concurrent `begin`s; free on a leader holding a lease |
| `Local` | the local `visible_ts` | none; may be stale by the replication lag; read-only transactions only |

- **What `Fresh` guarantees:** linearizable snapshot reads. A transaction sees every transaction acknowledged anywhere before it began. Serializable read-write transactions stay strictly serializable, as on one node.
- **What `Local` guarantees:** consistent but possibly stale snapshots, and a session's own writes, since replies wait for the local apply.
- **Validation limits.** A follower's read-write transaction is validated at the leader against history newer than its snapshot. Every replica keeps history at least as long as the maximum transaction age (ADR-0008's pin expiry), so any live transaction can be validated. An older one fails with `40001`.

### What stays per replica

- **Version GC** (ADR-0008) frees versions older than *local* pins only. All reads are local, so no replica needs another's old versions. GC's deletes are physical and not replicated.
- **Statistics** (ADR-0036): each replica analyzes its own copy and counts its own applied changes.
- **Residual and catalog caches** (ADR-0019, ADR-0031) invalidate on replicated versions, as designed.
- **Policy provers** subscribe to applied proposal rows on their own node (ADR-0030 phase 2).

### Snapshots, compaction and membership

- **Snapshots.** A replica's snapshot is a consistent fjall checkpoint of `data` and `meta` at an applied index, streamed to new or lagging replicas. The log is compacted up to the last index whose data writes are persisted.
- **Membership:**
  - changes use Raft's joint consensus, through an admin API (`Engine::add_member`, `remove_member`, `transfer_leadership`) that needs the `admin` privilege;
  - learners (non-voting replicas) catch up before becoming voters;
  - node IDs and addresses live in the Raft state, not in the catalog.

### Raft itself: openraft behind Interlaken's traits

- **The library.** openraft implements the protocol: elections, replication, joint consensus, snapshots, read index and leader leases. It is async and runs on Tokio, as the engine does, and it separates log storage, state machine and network behind traits that `interlaken-replication` implements.
- **Containing it.** Its types don't leak past that crate. If it doesn't suit, raft-rs or Interlaken's own implementation can replace it behind the same traits.
- **The log** lives in the `raftlog` keyspace of the same fjall database (ADR-0002), synced under `Durability::Sync`.

### The network

The question was whether the network layer should be a `Service`-style abstraction, as in tower's `Service<Request>`. The decision is to use tower at the edges, not in the core.

- **The core depends on a narrow transport trait of Interlaken's own.** `Transport` has one async method per message:
  - Raft's own messages: append entries, vote, install snapshot;
  - forwarded commits;
  - read-index requests;
  - membership operations.

  This is the seam deterministic testing needs, as ADR-0009's storage tests needed control over crashes. A transport trait with a few methods is easy to implement in memory, with delays, drops, reordering and partitions decided by a seeded scheduler. A generic `Service<Request>` stack is easy to compose but hard to simulate exactly. openraft's network trait is implemented over `Transport`.
- **The production transport is gRPC over HTTP/2 with tonic**, in a separate crate, `interlaken-net`, so embedded users don't build it. tonic is built on tower, so the cross-cutting concerns are tower layers:
  - per-message deadlines;
  - concurrency limits and load shedding;
  - metrics;
  - TLS through rustls.

  Retries apply only to idempotent messages: Raft's own messages and read-index requests. A forwarded commit is never retried blindly; a lost reply is `CommitOutcomeUnknown`, as above.
- **A future client server can share the stack.** pgwire, the engine's server mode and GraphQL could reuse the tower stack, but they're separate decisions.

### Security

- **Nodes authenticate each other with mutual TLS.** Certificates come from a cluster CA in the node configuration, and messages from unknown or unauthenticated peers are refused.
- **Every node is trusted with all data.** It holds a full replica, and policies are enforced where sessions run (ADR-0015), so replication doesn't weaken them.
- **Client authorization is unchanged.** A session on any node runs the same pipeline against the same replicated catalog and policies, and transactional authorization (ADR-0019) holds because policy rows are validated like any other read.
- **Membership changes need `admin`**, and are logged.

### Engine API

- **Opening a cluster member:** `Engine::open_cluster(dir, options, ClusterConfig { node_id, listen, peers, tls })`. A new cluster is bootstrapped from one node, with an empty or existing single-node database.
- **`Engine::open`** keeps opening a single-node database with `LocalSequencer`.
- **Sessions** don't change, except that `TxnOptions` gains `freshness`, and commit errors can include `CommitOutcomeUnknown` from leader changes. It gets the SQLSTATE Postgres uses for exactly this, `08007` (`transaction_resolution_unknown`), instead of today's `XX000`.
- **Status:** `Engine::cluster_status` reports the role, term, commit and applied indexes, and replication lag per follower.

## Testing

Replication is tested in layers, each extending the storage layer's tests (ADR-0009) rather than replacing them.

### 1. The log-based commit path passes the storage tests

Before any network exists, `RaftCommit` on one node passes everything `LocalSequencer` passes:
- the model tests;
- the `SIGKILL` crash harness;
- the LazyFS power-loss tests;
- the disk-space soak.

Both implementations of `CommitPath` run the same suites. Recovery adds two cases: replaying the log past the persisted applied index, and timestamps resuming correctly.

### 2. Simulated clusters with turmoil

Clusters of three and five nodes run inside [turmoil](https://github.com/tokio-rs/turmoil). It runs every host in one thread with simulated time, models the network between them, and draws faults from a seed.

- **The transport under test.** `Transport` is implemented over turmoil's TCP types, and the production gRPC transport runs over them too, so the simulation exercises the real network code, not a stand-in. A plain in-memory `Transport` remains for unit tests that don't need a network model.
- **Faults, from the seed:**
  - message latency, loss, holding and reordering;
  - symmetric and one-way partitions, including an isolated leader;
  - crashing and restarting hosts, from the same directory;
  - clock skew between hosts, which matters for leader leases;
  - membership changes and snapshot installs while all of the above is happening.
- **Workloads,** through real sessions on random nodes:
  - Elle's list-append and read-write-register transactions;
  - a bank-transfer workload whose total never changes;
  - DDL and policy changes mixed in.
- **Checks on every history:**
  - Elle finds it serializable, and strictly serializable for `Fresh` snapshots;
  - no acknowledged commit is lost;
  - a commit answered `CommitOutcomeUnknown` happens exactly once or not at all;
  - after the faults heal, every replica's data is identical at the same applied index;
  - `Local` snapshots are never older than the replication lag allows.
- **Reproducing failures.** Turmoil makes the network and time deterministic, but not Interlaken's own threads. fjall's storage pool and the commit sequencer run on real threads, and parts of the engine read `std::time`. So a failing seed usually reproduces but isn't guaranteed to. Each failure is reported with its seed and the full message history. Full determinism needs fjall with an injected scheduler, file system and clock (ADR-0001's "Option 2"), which is future work. madsim, which replaces Tokio, time and randomness across the whole process by patching dependencies, is the alternative if that fork never happens.

### 3. Crashes and power loss on each node

Raft is correct only if a node persists its term, its vote and its log entries before it replies: a node that forgets its vote after power loss can vote twice in one term. Turmoil doesn't model disks, so these cases use the existing harnesses:
- **Power loss:** every node's `raftlog` and data keyspaces on LazyFS, losing unsynced writes at random points, and then the checks of layer 2;
- **Crashes:** the `SIGKILL` harness extended to cluster members, the leader killed mid-commit included.

### 4. Real processes and networks, nightly

Separate processes talk over the gRPC transport, with faults injected by a userspace proxy such as toxiproxy, so CI needs no root for `tc` or `iptables`. They run the same workloads and checks, plus mutual-TLS cases: an unknown certificate refused, an expired one rejected. This job joins `nightly.yml` beside the durability jobs (backlog `nightly-durability-ci`).

### 5. A cluster answers as one node does

Without faults, the differential tests run through a three-node cluster with statements spread across nodes, and must match single-node results and the reference interpreter. That covers the DML, foreign key and cascade differential tests and the session oracles.

### 6. The layers above, on followers

Session tests on followers:
- a policy activated through one node is enforced on the others, and their residual caches are invalidated;
- a transaction's authorization conflicts with a concurrent policy change made through another node, as it does on one node (ADR-0019);
- provers see replicated proposals;
- a cluster bootstrapped from a single-node database keeps its data and its versions.

### 7. Benchmarks

These give ADR-0036's cost model and capacity planning their numbers:
- commit latency and throughput on three nodes, from the leader and from a follower;
- the cost of a `Fresh` snapshot;
- the replication lag of `Local` reads.

## Implementation Phases

- [x] **Phase 1: A log-based commit path on one node.**
  - `CommitPath` in storage;
  - the proposal and apply split;
  - the `raftlog` keyspace and log-derived timestamps;
  - recovery by replaying the log past the persisted applied index;
  - no network yet.
  - *Testing:* the storage model, crash and power-loss tests pass on this path too.
  - *As implemented:*
    - **The path is `StorageOptions::commit_log`**, off by default. Its default follows the environment variable `INTERLAKEN_COMMIT_LOG`, so every existing test, the crash and power-loss harnesses included, runs on the log path unchanged. Pull requests run the storage tests on both paths; the nightly job runs the whole workspace and the power-loss script on both.
    - **Proposal and apply.** The sequencer thread is the proposer. It validates exactly as before, then appends each group as one `LogEntry` to the `raftlog` keyspace, synced as `durability` says. That append is the commit.
      - An *apply* thread writes each entry's versions to `data` with `last_commit_ts` and a new `last_applied_index` in `meta`, unsynced, since the log is the durable record. It then publishes `visible_ts` and answers the callers.
      - The proposer validates the next group while earlier ones are applied, against a history that already holds them.
      - A *compaction* thread syncs and removes applied entries every 4,096 entries. It began inline in the apply thread every 256 entries, but its fsyncs then stalled commits: p99 rose to 10–17 ms, and under `Buffered`, which otherwise never syncs, throughput fell to a fifth.
    - **Timestamps are carried, not computed.** Each entry records the timestamp of its first transaction, and its `i`th transaction commits at that plus `i`, instead of the ADR's `base + index × 1024 + position`. It is just as deterministic, since every replica reads the same entry. It also leaves no gaps, has no 1,024-transaction cap, and lets a database move between paths without rebasing: a log entry simply continues after the last commit, whichever path made it.
    - **Recovery** applies every entry after `last_applied_index` when the database opens, whichever path it then uses. A gap in the log is corruption.
    - **`CommitPath` isn't a public trait yet.** Both paths are modes of the one sequencer, and the split that matters, proposal against apply, is in place. The public trait waits for phase 2, where its second implementation, in `interlaken-replication`, shows what it must expose: the validation history and the apply step.
  - *Testing:*
    - unit tests: entries round-trip and reject damage; unapplied entries are replayed once and in order; compaction keeps unapplied entries; gaps are refused;
    - `crates/storage/tests/commit_log.rs` covers switching between the paths four times across compactions, with timestamps continuing, and conflict detection through the log;
    - the whole storage suite on the log path;
    - `scripts/power-loss.sh` on the log path: under `Sync`, 50 power-loss cycles with 259,299 acknowledged commits and none lost; under `Buffered`, only losses that mode permits;
    - the whole workspace on the log path: 403 tests passed.
  - *Measured* (`commit_throughput`, blind writes, the development disk):

    | | 1 client | 16 | 64 | 256 |
    | --- | --- | --- | --- | --- |
    | `Sync`, single-node path | 3,916/s | 26,762/s | 78,854/s | 181,218/s |
    | `Sync`, commit log | 3,596/s | 19,206/s | 73,979/s | 188,300/s |
    | `Buffered`, single-node path | 89,683/s | 361,607/s | 300,290/s | 241,182/s |
    | `Buffered`, commit log | 52,471/s | 230,463/s | 188,080/s | 209,169/s |

    Under `Sync` the fsync dominates and the paths are close; p99 stays near 2 ms at 256 clients on both. Under `Buffered` the log path writes every commit twice, once to the log and once to the data, and reaches 58–87% of the single-node path. Compaction's occasional fsync still shows as a rare maximum latency of about 0.25 s under `Buffered`.
- [x] **Phase 2: Replication in process.**
  - `Transport`, its in-memory implementation and its turmoil implementation;
  - openraft integration;
  - leader validation, forwarding and `CommitOutcomeUnknown` on leader change;
  - deterministic three- and five-node clusters in one process.
  - *Testing:* layers 2 and 3 of [Testing](#testing).
  - *As implemented:*
    - **Storage's side** is the public module `interlaken_storage::replica`:
      - `CommitPath` is now a public trait. Transactions commit through it, and the sequencer implements it.
      - `CommitRequest` is public but opaque, with `encode` and `decode`, so a follower can forward it.
      - `FjallStorage::open_replicated` opens a database whose commit path the caller builds from a `Replica`.
      - A `Replica` applies committed batches and keeps the Raft log and state: a `replica_log` keyspace of entries by index, and `meta` entries for the vote, the committed and applied log IDs and the membership. The keyspace is separate from phase 1's `raftlog`, because Raft's entries carry terms and can be truncated.
      - A `Proposer` validates requests against the history, gives survivors consecutive timestamps, and returns them as one batch in phase 1's `LogEntry` format.
    - **Applying** writes a batch's versions, the entry's applied log ID and, for a membership entry, the membership, in one unsynced batch. A restarted replica therefore knows exactly which entries its data holds. The log is the durable record: under `Durability::Sync`, votes and appended entries are synced before they are acknowledged.
    - **Phantom proposals.** The leader records proposals in its validation history as it makes them, so later requests are validated against them. A leader that loses office may hold proposals that never commit. Each batch carries a random identifier in `LogEntry`'s index field, which a replicated batch doesn't otherwise use. When an entry applies, its identifier is compared with this replica's oldest outstanding proposal. A match means the history already holds the entry. Anything else means every outstanding proposal is a phantom: they are dropped from the history before the entry is recorded. This needs no assumption about when leadership changes are noticed.
    - **openraft 0.10.1, pinned.**
      - The log carries `Batch` (encoded bytes) and answers `()`.
      - Messages are postcard-encoded.
      - AppendEntries answers travel as `StreamAppendResult`, which drops openraft's conflict hint since the hint has no serialized form. A follower that diverges is found by binary search instead.
      - Snapshots are off (`SnapshotPolicy::Never`), so the log is never purged and grows until phase 4. The state machine and network refuse snapshot operations.
    - **The leader** runs a proposer task that takes queued requests in groups of up to 256. It proposes each group with `client_write_ff`, which enqueues in order, and answers the group once the entry commits and the leader has applied it.
      - A new leader proposes only after it has applied an entry of its own term, the blank entry each term begins with. Timestamps then continue from the last commit, and the history holds every earlier write.
      - If a proposal's outcome is lost, the leader proposes nothing more in that term: later proposals of the term might still commit, so their timestamps can't be reused.
    - **Followers** forward encoded requests to the leader. Once the leader answers with the entry's index, the follower waits until it has applied that entry, so its next transaction sees the commit.
      - A request that surely didn't arrive, or that the receiver refused because it doesn't lead, is retried until `CommitConfig::timeout` (5 s by default) runs out.
      - A request that may have arrived is never sent again.
    - **Errors.** `CommitOutcomeUnknown` now maps to SQLSTATE `08007` (`transaction_resolution_unknown`). It covers a lost reply, a leader change after proposing, and a commit that wasn't applied locally in time. The new `ReplicationUnavailable` maps to `57P03` (`cannot_connect_now`): there's no leader, or none could be reached before the timeout, and the commit surely didn't happen.
    - **Transactions on a follower read its applied state.** Histories are therefore serializable but not strictly serializable: a follower can begin before it has applied a commit that another node already acknowledged. Fresh reads, in phase 3, make them strict. Validation is still exact, because the leader's history covers every commit. If the leader trims history past an old follower snapshot, the transaction fails with `SnapshotTooOld`.
    - **Determinism.** Turmoil makes the network and time deterministic, but storage runs on real threads, as [Testing](#testing) foresaw. A seed usually reproduces a failure but isn't guaranteed to. Storage's real-time work also lets simulated time run ahead, so simulated replicas use longer timeouts: 200 ms heartbeats, elections after 1–2 s.
    - **Turmoil's transport lives in the tests.** Turmoil is a dev dependency, so its `Transport` (length-prefixed frames over turmoil's TCP, one connection per request) is part of the simulation tests, not a crate feature.
    - **Deferred from layer 2:**
      - Workloads use storage transactions directly: engine sessions aren't replicated until `Engine::open_cluster` in phase 5. So DDL and policy changes aren't mixed in yet.
      - Read-write-register workloads are deferred.
      - The faults are symmetric partitions of random minorities, crashes and restarts, and message latency. One-way partitions, message loss, clock skew, and membership changes or snapshot installs come with phases 3 and 4.
    - **Layer 3 moves to phase 5.** LazyFS power loss and the `SIGKILL` harness need replicas in separate processes, which phase 5 brings. Phase 5's testing already lists power loss on every node. In phase 2, the simulation crashes and restarts hosts from their directories, which is a process crash: unsynced writes stay in OS buffers.
  - *Testing:*
    - **Storage unit tests:** commit requests round-trip and reject damage; truncating the history restores earlier writes. `crates/storage/tests/replica.rs` covers proposals applying at consecutive timestamps, validation against outstanding proposals, and a phantom proposal dropped when another leader's entry applies.
    - **openraft's storage suite** passes, except the 11 tests that build or install snapshots, which wait for phase 4.
    - **`crates/replication/tests/cluster.rs`:** three- and five-node clusters over the in-memory transport. Commits from the leader and from followers reach every replica, and the committing node sees its commit at once. Conflicts span replicas. Five replicas agree under concurrent increments from every node. A new leader takes over when the leader crashes, and the old one catches up when it restarts. A minority can't commit, failing with `57P03` or `08007`, and converges after healing. Each test passed 20 consecutive runs.
    - **`crates/replication/tests/simulation.rs`:** turmoil clusters of three and five nodes, two clients per node, and a seeded schedule of partitions, crashes and restarts.
      - The list-append histories are checked by Elle as serializable. Every acknowledged append is in the final list on every replica, no failed append is, and an append whose outcome was unknown appears at most once.
      - The bank workload never changes the total any transaction reads or any replica holds.
      - After healing, every replica holds the same data at the same applied timestamp.
      - 20 seeds of each three-node workload and 20 of the five-node one ran clean locally. Pull requests run two seeds each; the nightly job runs 50.
- [x] **Phase 3: Reads.**
  - read index and leases;
  - `freshness`;
  - read-your-writes through local apply;
  - history retention for follower transactions.
  - *Testing:* linearizability checks of `Fresh` snapshots in the simulation; staleness bounds of `Local`.
  - *As implemented:*
    - **`TxnOptions::freshness`** takes `Freshness::Fresh`, the default, or `Freshness::Local`. `Local` applies only to read-only transactions: a read-write transaction is always fresh, rather than refused, so every writer stays strictly serializable. A single-node database ignores the option.
    - **The barrier is `CommitPath::read_barrier`.** `begin` calls it before taking its snapshot. It is a no-op by default, so the sequencer needs no change.
      - The Raft commit path asks for a read index, then waits until this replica has applied it. openraft's `get_read_log_id` supplies the index: `max(the term's blank entry, the commit index)`, once leadership is confirmed.
      - Under READ COMMITTED only the first snapshot is fresh. Later statements take the local `visible_ts`, which never goes backwards.
    - **Leases.**
      - With `CommitConfig::lease_reads`, on by default, the leader confirms its leadership from a quorum's acknowledgement within its lease, openraft's election timeout. Without a recent enough acknowledgement, it sends heartbeats at once.
      - openraft's voters refuse to elect a new leader while they have heard from the current one within its lease, so a leader whose lease holds is still the only leader. This assumes clock drift between nodes is small against the election timeout.
      - Turning `lease_reads` off makes every read index wait for a fresh round of heartbeats.
    - **Batching.** A reader task on each node answers every request queued so far with one read index. Each request was queued before the index was obtained, so the index covers everything acknowledged before any of them.
      - A follower sends one `ReadIndex` request per batch to the leader, which batches it with its own.
      - Read-index requests are idempotent, so any failure is retried until the commit timeout. After that, `begin` fails with `57P03`.
    - **Read-your-writes** came with phase 2: a commit returns only once its node has applied it, so a local snapshot on that node sees it.
    - **History retention.** A replica keeps every commit it applied within `ReplicaConfig::history_retention`, a minute by default, as well as what local pins need. Any replica that becomes leader can then validate a follower's transaction whose snapshot is that old. The ADR asked for the maximum transaction age, five minutes by default. A busy leader would hold five minutes of written keys in memory, though, so the retention is its own setting. A transaction whose snapshot is older than the retention fails validation with `40001`, as the ADR intends.
  - *Testing:*
    - `crates/replication/tests/reads.rs`:
      - fresh reads on every node see a commit acknowledged on another node just before, with and without leases;
      - a node cut off from the majority serves stale local reads but refuses fresh ones (`57P03`), its local reads catch up within moments of healing, and it reads its own commits locally;
      - a deposed leader refuses fresh reads once the majority has elected a successor and committed;
      - a follower's transaction validates against history newer than its snapshot, and fails with `40001` when retention is zero.

      Each test passed 20 consecutive runs.
    - **The simulation** now checks fresh list-append histories, of three and five nodes, as strictly serializable under Elle. A new workload gives read-only transactions local snapshots. Elle finds its histories strongly session-serializable: they keep each client's own order, and lose and duplicate nothing. It also finds them not strictly serializable, which shows the check can fail and that fresh snapshots are what prevent it.
  - *Measured* (`measure_fresh_snapshots`, release build, in-memory transport, so these are overheads without network latency):
    - a lone fresh `begin` costs 10–13 µs with leases and 23–27 µs without, against 0.2 µs for a local one;
    - batching lets 64 concurrent clients reach 0.8–1.2 million fresh begins a second, close to local ones.
- [ ] **Phase 4: Snapshots, compaction and membership.**
  - fjall checkpoints streamed as snapshots;
  - log compaction;
  - joint-consensus membership with learners;
  - the admin API.
  - *Testing:* membership changes under faults in the simulation, and a replica rebuilt from a snapshot matching the leader's data.
- [ ] **Phase 5: Production network.**
  - `interlaken-net` with tonic, mTLS and tower layers;
  - `Engine::open_cluster`;
  - cluster status and metrics.
  - *Testing:* a multi-process nightly job with real network faults, and power-loss tests of the log on every node (the LazyFS harness, ADR-0009).
- [ ] **Phase 6: Layers above.**
  - provers watching replicated proposals (ADR-0030 phase 2);
  - per-replica statistics (ADR-0036);
  - the book's operations chapters on running a cluster.

## Consequences

### Positive

- **Availability and durability:** losing a minority of nodes loses no acknowledged commit and keeps the cluster writable.
- **Little changes above storage.** Transactions, validation, MVCC and everything above storage keep their meaning: the sequencer's logic moves to the leader rather than being redesigned.
- **Reads scale across replicas.** `Local` snapshots cost no coordination.
- **Embedded use is untouched.** Single-node databases keep the local sequencer with no new dependencies.
- **Simulation-tested.** The narrow transport lets replication run under turmoil's simulated network, with the production gRPC code inside it.

### Negative

- **All writes go through one leader.** Write throughput is bounded by one node's validation and one consensus round per group, as one sequencer bounded it before. Sharding is future work.
- **Commits cost a network round trip** to a majority, plus one more from a follower to the leader. Fresh snapshots add a read-index round trip.
- **New failure modes:** commits with unknown outcomes on leader changes, and follower transactions failing validation because their snapshot predates the leader's history.
- **A large new dependency** (openraft) and, for clusters, a network stack (tonic, rustls).
- **Operational complexity:** certificates, membership, snapshots and monitoring of replication lag.

## Open Questions

- Should `Fresh` be the default for read-only transactions too, or should they default to `Local` and opt into `Fresh`?
- Is `index × 1024 + position` the right timestamp scheme, or should timestamps leave room for hybrid logical clocks (ADR-0002's note) in case geo-distribution follows?
- Should a follower forward each commit as it arrives, or batch forwarded commits to save round trips at the cost of latency?
- How should the engine's server mode be packaged: a binary in this workspace, or left to embedders until pgwire exists?

## Future Work

- Range sharding with one Raft group per range, a timestamp oracle and cross-range transactions (ADR-0001).
- Follower reads at a bounded staleness for geo-distributed replicas.
- pgwire and a client-facing server mode sharing the tower stack.
- Online backup from a replica's snapshot.
- Fully deterministic simulation, by forking fjall with an injected scheduler, file system and clock (ADR-0001's "Option 2"), or by moving to madsim.

## References

- [ADR-0001](0001-fjall-storage-engine-and-async-sync-boundary.md), [ADR-0002](0002-engine-owned-mvcc-and-keyspace-layout.md), [ADR-0004](0004-optimistic-transactions-and-isolation-levels.md), [ADR-0005](0005-commit-sequencer-group-commit-and-durability.md), [ADR-0008](0008-version-garbage-collection.md), [ADR-0009](0009-storage-correctness-testing-strategy.md), [ADR-0015](0015-security-core-and-policy-provider-contract.md), [ADR-0019](0019-transactional-authorization-and-residual-cache.md), [ADR-0022](0022-vectorized-arrow-stream-executor.md), [ADR-0030](0030-policy-proposals-and-distributed-verification.md), [ADR-0031](0031-engine-sessions-and-storage-backed-catalog.md), [ADR-0036](0036-statistics-and-cost-based-planning.md)
- Backlog `pipelined-commit-validation`, `prover-subscriptions`, `nightly-durability-ci`.
- D. Ongaro and J. Ousterhout, "In Search of an Understandable Consensus Algorithm (Raft)", USENIX ATC 2014, and Ongaro's thesis (read index, leases, joint consensus).
- openraft, tonic, tower, turmoil and madsim documentation; Elle (Kingsbury and Alvaro, "Elle: Inferring Isolation Anomalies from Experimental Observations", VLDB 2020).
