# Running a cluster

A database can run on several servers at once, each holding a full copy, kept identical by the Raft consensus protocol ([ADR-0037](../reference/design-records.md)). The cluster survives the loss of any minority of its members, and every member answers queries. This chapter covers building, securing, forming and operating one.

## How a cluster works

- **One leader validates commits.** Any member runs transactions, but every commit goes to the leader. The leader checks it for conflicts exactly as a single node would, gives it a commit timestamp, and puts it in the replicated log. Once a majority of members has stored the entry, it is committed and every member applies it.
- **Reads are local.** A transaction reads its own member's copy. By default its snapshot is *fresh*: the member first asks the leader how far the log is committed, and waits until it has applied that much. So a transaction sees every commit acknowledged anywhere before it began. Concurrent transactions share one request to the leader, and a leader confirms its own position from its lease without asking anyone.
- **A commit returns once its own member has applied it,** so the session's next statement there sees it.
- **Everything else is per member:** garbage collection, caches and, once ADR-0036 is built, statistics. Policies, the catalog and privileges are ordinary tables, so they replicate with everything else.

## Building

Cluster support is the engine's `cluster` feature, which embedded builds leave out:

```toml
interlaken-engine = { version = "0.1", features = ["cluster"] }
```

It brings in the `interlaken-net` crate: gRPC over HTTP/2 through tonic, with TLS from rustls.

## Certificates

Members authenticate each other with mutual TLS. A member refuses any peer without a certificate signed by the cluster's certificate authority, and refuses expired certificates. Every member needs:

- the CA's certificate;
- its own certificate, signed by the CA, carrying the DNS name `interlaken` (or the name set in `TlsConfig::server_name`);
- its private key.

All three are PEM files. Every member holds all the data, so certificates don't distinguish members; any member's certificate admits it.

## Forming a cluster

One member *bootstraps*: it forms a cluster of itself. Given an administrator, it creates a new database, or it keeps the database already in its directory, so a single-node database can grow into a cluster.

```rust,ignore
use interlaken_engine::{Engine, EngineOptions};
use interlaken_net::{ClusterConfig, TlsConfig};

let tls = TlsConfig::new(ca_pem, node_cert_pem, node_key_pem);
let mut config = ClusterConfig::new(1, "0.0.0.0:7400".parse()?, tls);
config.advertise = Some("db1.internal:7400".into());
config.bootstrap = true;
let engine = Engine::open_cluster("/var/lib/interlaken", EngineOptions::default(), config, Some(&admin)).await?;
```

Every other member opens without `bootstrap` and waits to be added. An administrator then adds each one from a session on any member:

```rust,ignore
let mut root = engine.session(SecurityContext::new(admin));
root.add_member(2, "db2.internal:7400").await?;
root.add_member(3, "db3.internal:7400").await?;
```

A new member joins as a *learner*: it receives a snapshot of the data, catches up on the log, and only then becomes a voter. The cluster stays available throughout, since membership changes use Raft's joint consensus. Three or five voters are typical: three survive one failure, five survive two.

A restarted member rejoins on its own, from its directory. `remove_member` takes a member out of the voters, and `transfer_leadership` hands leadership to a chosen voter, for example before maintenance on the leader. All three need the `admin` privilege.

## Status

`Engine::cluster_status` reports this member's view of the cluster:

| Field | Meaning |
| --- | --- |
| `role` | `leader`, `follower`, `candidate` or `learner` |
| `term`, `leader` | the current Raft term and the member believed to lead |
| `last_log_index`, `committed_index`, `applied_index` | how far this member's log reaches, is known committed, and is applied |
| `snapshot_index` | the log index the latest snapshot covers |
| `voters`, `learners` | the latest membership |
| `lag` | on the leader, how many entries each member trails by |

`Engine::cluster()` gives the member itself; its `net_metrics()` counts requests served and, for each peer, requests sent and failed.

## Errors a cluster adds

| SQLSTATE | Meaning | What to do |
| --- | --- | --- |
| `40001` | A conflict, as on one node | Retry the transaction |
| `57P03` | No leader could be reached in time; the commit surely didn't happen | Retry once the cluster has a leader |
| `08007` | The commit may or may not have happened: the leader changed, or the reply was lost | Check whether it took effect before retrying |

A member cut off from the majority can't commit or begin fresh transactions; both fail with `57P03` after the commit timeout. It can still serve local reads.

## Fresh and local snapshots

`TxnOptions::freshness` chooses a transaction's snapshot:

- **`Freshness::Fresh`** (the default): linearizable, as described above. Serializable transactions stay strictly serializable across the cluster.
- **`Freshness::Local`**: the member's applied state, with no request to the leader. It may lag by the replication delay, but it never goes backwards for a session and always includes the session's own commits. It applies to read-only transactions; read-write ones are always fresh.

## Durability

Under `Durability::Sync`, each member syncs its log and its Raft vote before acknowledging them, so no acknowledged commit is lost even if every member loses power at once. Applying committed entries to the data is not synced: the log is the durable record, and the data is synced before the log is compacted. Under `Durability::Buffered`, a power loss on a majority of members at once can lose acknowledged commits.

## Settings

`ClusterConfig::replica` holds the Raft settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `heartbeat` | 50 ms | how often the leader heartbeats |
| `election_min`, `election_max` | 150–300 ms | how long followers wait without a leader before standing for election; raise both on loaded or distant networks |
| `commit.timeout` | 5 s | how long a commit or fresh snapshot waits for a leader |
| `commit.lease_reads` | on | serve read indexes from the leader's lease; assumes clock drift is small against the election timeout |
| `history_retention` | 60 s | how long applied commits stay available for validating transactions begun on other members; older ones fail with `40001` |
| `snapshot_every`, `logs_kept` | 10,000, 1,000 | compact the log every so many entries, keeping the most recent; snapshots themselves are made only when a member needs one, from current data |
| `snapshot_chunk_bytes` | 1 MiB | the size of the chunks snapshots are sent in; memory used for a snapshot stays near this, whatever the database's size |

`ClusterConfig` itself sets the request deadline between members (5 s), the concurrent requests served per connection (256), and `peers`: addresses that take precedence over the ones membership records, for members reached through a proxy or another network.

## Policy provers

In `proof-required` mode ([Changing policies](../security/proposals.md)), a member with an SMT prover can serve the whole cluster. `policy::watch_proposals` runs a prover whenever its member applies new commits. Proposals submitted on any member arrive by replication, and the prover's results replicate back.

## Limits

- A member installing a snapshot holds its old data and the new copy on disk until the switch.
- Commits made through a follower are slower than through the leader under moderate concurrency.
- Membership changes aren't written to an audit log.

## Testing

[Testing durability](durability-testing.md) describes the simulated clusters, the multi-process fault tests and power loss on every member that check replication.
