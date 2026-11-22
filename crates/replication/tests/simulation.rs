//! Replicated clusters under a simulated network, with turmoil (ADR-0037
//! phases 2 and 3, testing layer 2).
//!
//! Each replica is a turmoil host talking to the others over turmoil's TCP,
//! through a [`Transport`] that frames requests on the wire. Workloads run
//! on every node while a schedule drawn from a seed isolates nodes, heals
//! partitions, and crashes and restarts hosts from their directories. Then:
//!
//! - **list-append:** with fresh snapshots, Elle checks the history is
//!   strictly serializable; every acknowledged append is in the final list
//!   on every replica, no failed append is, and each append whose outcome
//!   was unknown appears at most once.
//! - **local reads:** read-only transactions take local snapshots, which
//!   may be stale; Elle checks the history is still serializable and keeps
//!   each client's own order (strong session serializability).
//! - **bank:** transfers between accounts never change the total that any
//!   transaction reads, or that each replica holds at the end.
//! - **convergence:** after the faults heal, every replica holds the same
//!   data at the same applied timestamp.
//!
//! Turmoil makes the network and time deterministic, but storage runs on
//! real threads, so a seed usually reproduces a failure but isn't
//! guaranteed to. Failures report their seed. `SIM_SEEDS` sets how many
//! seeds each test runs (default 2), `SIM_SEED` runs one seed. Elle needs
//! Java and `elle-cli`, as in the storage crate's Elle tests; without them
//! the other checks still run.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use interlaken_replication::{
    ReplicaConfig, ReplicaNode, Request, Response, Transport, TransportError,
};
use interlaken_storage::{
    Durability, Error, FjallStorage, Freshness, Storage, StorageOptions, TxnOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::net::{TcpListener, TcpStream};

/// The port every replica listens on.
const PORT: u16 = 7000;
/// Client processes per replica.
const PROCESSES: u64 = 2;

/// Return the host name of node `id`.
fn host(id: u64) -> String {
    format!("n{id}")
}

/// Write one length-prefixed frame.
async fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    stream
        .write_u32(u32::try_from(bytes.len()).unwrap())
        .await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

/// Read one length-prefixed frame.
async fn read_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let len = stream.read_u32().await?;
    let mut bytes = vec![0; len as usize];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

/// Requests over turmoil's TCP: one connection per request.
struct TurmoilTransport;

impl Transport for TurmoilTransport {
    /// Connect to `target`, send the framed request and read the framed response.
    async fn send(&self, target: u64, request: Request) -> Result<Response, TransportError> {
        let not_delivered = |e: std::io::Error| TransportError::NotDelivered(e.to_string());
        let no_reply = |e: std::io::Error| TransportError::NoReply(e.to_string());
        let bytes = postcard::to_stdvec(&request).unwrap();
        let mut stream = tokio::time::timeout(
            Duration::from_secs(1),
            TcpStream::connect((host(target), PORT)),
        )
        .await
        .map_err(|_| TransportError::NotDelivered("connect timed out".into()))?
        .map_err(not_delivered)?;
        let exchange = async {
            write_frame(&mut stream, &bytes).await?;
            read_frame(&mut stream).await
        };
        let reply = tokio::time::timeout(Duration::from_secs(5), exchange)
            .await
            .map_err(|_| TransportError::NoReply("timed out".into()))?
            .map_err(no_reply)?;
        postcard::from_bytes(&reply).map_err(|e| TransportError::NoReply(e.to_string()))
    }
}

/// Answer framed requests on one connection until it closes.
async fn serve(mut stream: TcpStream, handler: Arc<dyn interlaken_replication::Handler>) {
    while let Ok(bytes) = read_frame(&mut stream).await {
        let Ok(request) = postcard::from_bytes::<Request>(&bytes) else {
            return;
        };
        let response = handler.handle(request).await;
        if write_frame(&mut stream, &postcard::to_stdvec(&response).unwrap())
            .await
            .is_err()
        {
            return;
        }
    }
}

/// A replica's visible timestamp and its lists or balances by key.
type Report = (u64, BTreeMap<u64, Vec<i64>>);

/// One micro-operation of a transaction, with the read result once known.
#[derive(Clone, Debug)]
enum Mop {
    /// Append a value to a list.
    Append(u64, i64),
    /// Read a whole list.
    Read(u64, Option<Vec<i64>>),
    /// Transfer an amount between two accounts (bank workload).
    Transfer(u64, u64, i64),
    /// Read every account's balance (bank workload).
    Balances(Option<Vec<i64>>),
}

/// Render list-append micro-operations as an EDN vector.
fn edn(mops: &[Mop]) -> String {
    let mut out = String::from("[");
    for (i, mop) in mops.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match mop {
            Mop::Append(k, v) => write!(out, "[:append {k} {v}]").unwrap(),
            Mop::Read(k, None) => write!(out, "[:r {k} nil]").unwrap(),
            Mop::Read(k, Some(list)) => {
                let items: Vec<String> = list.iter().map(i64::to_string).collect();
                write!(out, "[:r {k} [{}]]", items.join(" ")).unwrap();
            }
            Mop::Transfer(..) | Mop::Balances(_) => unreachable!("bank operations aren't EDN"),
        }
    }
    out.push(']');
    out
}

/// How a transaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Committed.
    Ok,
    /// Surely didn't commit.
    Fail,
    /// May or may not have committed.
    Info,
}

/// A transaction's invocation and, once known, its completion.
#[derive(Clone, Debug)]
struct Op {
    /// The client process.
    process: u64,
    /// The micro-operations as invoked.
    invoked: Vec<Mop>,
    /// The completion, with read results, if one came.
    completed: Option<(Kind, Vec<Mop>)>,
}

/// The shared record of a simulation: operations in real-time order, and
/// each replica's final state.
#[derive(Default)]
struct History {
    /// Every operation, by index of invocation, with history order.
    ops: Mutex<Vec<Op>>,
    /// Invocations and completions, as (op index, is completion), in order.
    events: Mutex<Vec<(usize, bool)>>,
    /// The next value to append, unique across the run.
    next_value: AtomicI64,
    /// The next client process ID.
    next_process: AtomicU64,
    /// Transactions started so far.
    issued: AtomicU64,
    /// Each replica's latest report: its visible timestamp and data.
    reports: Mutex<BTreeMap<u64, Report>>,
    /// Set once the workload is over and the network healed.
    quiet: AtomicBool,
    /// Failed commits by SQLSTATE.
    failures: Mutex<BTreeMap<String, u64>>,
    /// Faults injected, by kind.
    faults: Mutex<BTreeMap<&'static str, u64>>,
    /// Replicas the cluster starts with.
    nodes: u64,
    /// Hosts in the simulation: the replicas, and one that joins later.
    hosts: u64,
    /// The voters, as membership changes leave them.
    members: Mutex<BTreeSet<u64>>,
    /// How far the membership changes have got: 0 none, 1 learner added,
    /// 2 node 1 replaced by the new node.
    membership_step: AtomicU64,
    /// The highest log index a replica reported a snapshot of.
    snapshot_index: AtomicU64,
}

impl History {
    /// Record an invocation and return its index.
    fn invoke(&self, process: u64, mops: Vec<Mop>) -> usize {
        let mut ops = self.ops.lock().unwrap();
        ops.push(Op {
            process,
            invoked: mops,
            completed: None,
        });
        let index = ops.len() - 1;
        self.events.lock().unwrap().push((index, false));
        index
    }

    /// Record a completion.
    fn complete(&self, index: usize, kind: Kind, mops: Vec<Mop>) {
        self.ops.lock().unwrap()[index].completed = Some((kind, mops));
        self.events.lock().unwrap().push((index, true));
    }
}

/// The workload a simulation runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    /// Elle's list-append, every snapshot fresh.
    ListAppend,
    /// Elle's list-append, with local snapshots for read-only transactions.
    LocalReads,
    /// Elle's list-append, every snapshot fresh, while a new node joins and
    /// replaces node 1.
    Membership,
    /// Transfers between accounts.
    Bank,
}

/// Distinct lists, or accounts.
const KEYS: u64 = 8;
/// Each account's opening balance.
const OPENING: i64 = 100;

/// Return the storage key of list or account `k`.
fn key(k: u64) -> Bytes {
    Bytes::from(format!("k{k}"))
}

/// Encode a list of integers.
fn encode(list: &[i64]) -> Bytes {
    Bytes::from(
        list.iter()
            .flat_map(|v| v.to_be_bytes())
            .collect::<Vec<u8>>(),
    )
}

/// Decode a list of integers.
fn decode(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|c| i64::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

/// Read every key's list at one snapshot.
async fn read_all(storage: &FjallStorage) -> Result<Report, Error> {
    let txn = storage.begin(TxnOptions::default().read_only()).await?;
    let values = txn.get_many((0..KEYS).map(key).collect()).await?;
    let data = values
        .iter()
        .enumerate()
        .filter_map(|(k, v)| v.as_deref().map(|v| (k as u64, decode(v))))
        .collect();
    Ok((txn.snapshot().into_raw(), data))
}

/// Run one transaction of `mops` on `storage`, returning the completed
/// micro-operations or the error.
async fn run_txn(storage: &FjallStorage, mops: &[Mop], local: bool) -> Result<Vec<Mop>, Error> {
    let reads_only = mops
        .iter()
        .all(|m| matches!(m, Mop::Read(..) | Mop::Balances(_)));
    let options = if local && reads_only {
        TxnOptions::default()
            .read_only()
            .freshness(Freshness::Local)
    } else {
        TxnOptions::default()
    };
    let txn = storage.begin(options).await?;
    let mut done = Vec::new();
    for mop in mops {
        match mop {
            Mop::Append(k, v) => {
                let mut list = txn.get_many(vec![key(*k)]).await?[0]
                    .as_deref()
                    .map(decode)
                    .unwrap_or_default();
                list.push(*v);
                txn.put(key(*k), encode(&list))?;
                done.push(mop.clone());
            }
            Mop::Read(k, _) => {
                let list = txn.get_many(vec![key(*k)]).await?[0]
                    .as_deref()
                    .map(decode)
                    .unwrap_or_default();
                done.push(Mop::Read(*k, Some(list)));
            }
            Mop::Transfer(from, to, amount) => {
                let values = txn.get_many(vec![key(*from), key(*to)]).await?;
                let balance = |v: &Option<Bytes>| v.as_deref().map_or(OPENING, |b| decode(b)[0]);
                let (a, b) = (balance(&values[0]), balance(&values[1]));
                txn.put(key(*from), encode(&[a - amount]))?;
                txn.put(key(*to), encode(&[b + amount]))?;
                done.push(mop.clone());
            }
            Mop::Balances(_) => {
                let values = txn.get_many((0..KEYS).map(key).collect()).await?;
                let balances = values
                    .iter()
                    .map(|v| v.as_deref().map_or(OPENING, |b| decode(b)[0]))
                    .collect();
                done.push(Mop::Balances(Some(balances)));
            }
        }
        txn.next_statement();
    }
    txn.commit().await?;
    Ok(done)
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    /// Return the next pseudo-random number.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Run client transactions on `storage` until the workload has issued `total`.
async fn client(
    storage: FjallStorage,
    history: Arc<History>,
    workload: Workload,
    total: u64,
    seed: u64,
) {
    let process = history.next_process.fetch_add(1, Ordering::Relaxed);
    let mut process = process;
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    while history.issued.fetch_add(1, Ordering::Relaxed) < total {
        // Think time, so the workload spans many faults.
        tokio::time::sleep(Duration::from_millis(rng.next() % 500)).await;
        let mops: Vec<Mop> = match workload {
            Workload::ListAppend | Workload::LocalReads | Workload::Membership => {
                (0..1 + rng.next() % 3)
                    .map(|_| {
                        let k = rng.next() % KEYS;
                        if rng.next().is_multiple_of(2) {
                            Mop::Append(k, history.next_value.fetch_add(1, Ordering::Relaxed))
                        } else {
                            Mop::Read(k, None)
                        }
                    })
                    .collect()
            }
            Workload::Bank => {
                if rng.next().is_multiple_of(3) {
                    vec![Mop::Balances(None)]
                } else {
                    let from = rng.next() % KEYS;
                    let to = (from + 1 + rng.next() % (KEYS - 1)) % KEYS;
                    vec![Mop::Transfer(from, to, 1 + (rng.next() % 10) as i64)]
                }
            }
        };
        let index = history.invoke(process, mops.clone());
        match run_txn(&storage, &mops, workload == Workload::LocalReads).await {
            Ok(done) => history.complete(index, Kind::Ok, done),
            Err(e) if matches!(e.sqlstate(), "40001" | "57P03") => {
                *history
                    .failures
                    .lock()
                    .unwrap()
                    .entry(e.sqlstate().to_owned())
                    .or_default() += 1;
                history.complete(index, Kind::Fail, mops);
            }
            Err(e) => {
                *history
                    .failures
                    .lock()
                    .unwrap()
                    .entry(e.sqlstate().to_owned())
                    .or_default() += 1;
                history.complete(index, Kind::Info, mops);
                // An indeterminate process never continues (Elle's convention).
                process = history.next_process.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Settings for replicas under simulated time: storage on real threads
/// takes real time while simulated time races ahead, so timeouts are long.
fn config(id: u64) -> ReplicaConfig {
    let mut config = ReplicaConfig::new(id);
    config.heartbeat = Duration::from_millis(200);
    config.election_min = Duration::from_millis(1000);
    config.election_max = Duration::from_millis(2000);
    config.commit.timeout = Duration::from_secs(20);
    config.commit.retry = Duration::from_millis(100);
    // Frequent snapshots, so lagging and new replicas catch up from them.
    config.snapshot_every = 30;
    config.logs_kept = 10;
    config
}

/// Open node `id` from `dir`, retrying while a crashed predecessor releases it.
async fn open(id: u64, dir: PathBuf) -> ReplicaNode<TurmoilTransport> {
    for _ in 0..200 {
        let options = StorageOptions {
            durability: Durability::Buffered,
            ..StorageOptions::default()
        };
        if let Ok(node) =
            ReplicaNode::open(dir.clone(), options, config(id), TurmoilTransport).await
        {
            return node;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("node {id} couldn't reopen its storage");
}

/// The body of host `id`: open the replica, serve peers, run clients and
/// report the replica's data.
async fn replica(
    id: u64,
    dir: PathBuf,
    history: Arc<History>,
    workload: Workload,
    total: u64,
    seed: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let node = Arc::new(open(id, dir).await);
    if id == 1 {
        // Fails harmlessly after the first boot: the cluster exists.
        let _ = node
            .initialize(&(1..=history.nodes).collect::<Vec<_>>())
            .await;
    }
    let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
    let handler = node.handler();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream, Arc::clone(&handler)));
        }
    });
    if workload == Workload::Membership && id != history.hosts {
        tokio::spawn(change_membership(Arc::clone(&node), Arc::clone(&history)));
    }
    for p in 0..PROCESSES {
        let boot = history.next_process.load(Ordering::Relaxed);
        tokio::spawn(client(
            node.storage().clone(),
            Arc::clone(&history),
            workload,
            total,
            seed ^ (id << 32) ^ (p << 16) ^ boot,
        ));
    }
    loop {
        if let Ok(report) = read_all(node.storage()).await {
            history.reports.lock().unwrap().insert(id, report);
        }
        if let Some(index) = node.status().snapshot_index {
            history.snapshot_index.fetch_max(index, Ordering::Relaxed);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Make the membership changes, from whichever replica gets there first:
/// add the new node as a learner, then make it a voter in node 1's place.
async fn change_membership(node: Arc<ReplicaNode<TurmoilTransport>>, history: Arc<History>) {
    let new = history.hosts;
    tokio::time::sleep(Duration::from_secs(4)).await;
    loop {
        match history.membership_step.load(Ordering::Acquire) {
            0 => {
                if node.add_learner(new).await.is_ok() {
                    let _ = history.membership_step.compare_exchange(
                        0,
                        1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
            1 => {
                let voters: Vec<u64> = (2..=new).collect();
                if node.set_voters(&voters).await.is_ok() {
                    *history.members.lock().unwrap() = voters.into_iter().collect();
                    let _ = history.membership_step.compare_exchange(
                        1,
                        2,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            }
            _ => return,
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Return the latest reports of the current voters.
fn member_reports(history: &History) -> BTreeMap<u64, Report> {
    let members = history.members.lock().unwrap().clone();
    history
        .reports
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _)| members.contains(id))
        .map(|(id, report)| (*id, report.clone()))
        .collect()
}

/// Wait in simulated time until the replicas report identical data.
async fn converge(history: Arc<History>) -> Result<(), Box<dyn std::error::Error>> {
    let mut agreed = 0;
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !history.quiet.load(Ordering::Acquire) {
            agreed = 0;
            continue;
        }
        let reports = member_reports(&history);
        let same = reports.len() == history.members.lock().unwrap().len()
            && reports
                .values()
                .all(|r| r == reports.values().next().unwrap());
        agreed = if same { agreed + 1 } else { 0 };
        if agreed >= 6 {
            return Ok(());
        }
    }
    Err(format!(
        "replicas never converged: {:?}",
        history.reports.lock().unwrap()
    )
    .into())
}

/// Run one simulation of `workload` on `nodes` replicas from `seed`,
/// returning its history.
fn simulate(workload: Workload, nodes: u64, seed: u64) -> Arc<History> {
    let hosts = nodes + u64::from(workload == Workload::Membership);
    let history = Arc::new(History {
        nodes,
        hosts,
        members: Mutex::new((1..=nodes).collect()),
        ..History::default()
    });
    let dirs: Vec<tempfile::TempDir> = (0..hosts).map(|_| tempfile::tempdir().unwrap()).collect();
    let total = 400;
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(3600))
        .rng_seed(seed)
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(20))
        .build();
    for id in 1..=hosts {
        let dir = dirs[(id - 1) as usize].path().to_path_buf();
        let history = Arc::clone(&history);
        sim.host(host(id), move || {
            replica(id, dir.clone(), Arc::clone(&history), workload, total, seed)
        });
    }
    sim.client("checker", converge(Arc::clone(&history)));
    let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
    let mut crashed: Option<u64> = None;
    let mut isolated = false;
    let mut next_fault = Duration::from_secs(1);
    loop {
        let finished = sim.step().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        if finished {
            break;
        }
        if history.quiet.load(Ordering::Acquire) {
            continue;
        }
        if sim.elapsed() < next_fault {
            continue;
        }
        next_fault = sim.elapsed() + Duration::from_millis(1000 + rng.next() % 2000);
        let changing =
            history.membership_step.load(Ordering::Acquire) < 2 && workload == Workload::Membership;
        if history.issued.load(Ordering::Relaxed) >= total + PROCESSES * hosts && !changing {
            // The workload is over: heal everything and let the replicas converge.
            heal(&sim, hosts);
            if let Some(id) = crashed.take() {
                sim.bounce(host(id));
            }
            history.quiet.store(true, Ordering::Release);
            continue;
        }
        let fault = |name| *history.faults.lock().unwrap().entry(name).or_default() += 1;
        match rng.next() % 4 {
            0 if !isolated => {
                fault("isolate");
                // A random minority, cut off from the rest.
                let mut minority = BTreeSet::new();
                for _ in 0..=(rng.next() % ((nodes - 1) / 2)) {
                    minority.insert(1 + rng.next() % hosts);
                }
                for &id in &minority {
                    for other in (1..=hosts).filter(|o| !minority.contains(o)) {
                        sim.partition(host(id), host(other));
                    }
                }
                isolated = true;
            }
            1 => {
                fault("heal");
                heal(&sim, hosts);
                isolated = false;
            }
            2 if crashed.is_none() => {
                let id = 1 + rng.next() % hosts;
                sim.crash(host(id));
                fault("crash");
                crashed = Some(id);
            }
            _ => {
                if let Some(id) = crashed.take() {
                    fault("restart");
                    sim.bounce(host(id));
                }
            }
        }
    }
    eprintln!(
        "seed {seed}: {:?} simulated, faults {:?}, failures {:?}, latest snapshot {}, voters {:?}",
        sim.elapsed(),
        history.faults.lock().unwrap(),
        history.failures.lock().unwrap(),
        history.snapshot_index.load(Ordering::Relaxed),
        history.members.lock().unwrap()
    );
    drop(sim);
    history
}

/// Repair every link between the `nodes` replicas.
fn heal(sim: &turmoil::Sim<'_>, nodes: u64) {
    for a in 1..=nodes {
        for b in (1..=nodes).filter(|&b| b != a) {
            sim.repair(host(a), host(b));
        }
    }
}

/// Return the seeds to run: `SIM_SEED`, or `SIM_SEEDS` seeds from 1.
fn seeds() -> Vec<u64> {
    if let Some(seed) = std::env::var("SIM_SEED").ok().and_then(|v| v.parse().ok()) {
        return vec![seed];
    }
    let count: u64 = std::env::var("SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    (1..=count).collect()
}

/// Assert the voters' final data is identical, and return it.
fn final_data(history: &History, seed: u64) -> BTreeMap<u64, Vec<i64>> {
    let reports = member_reports(history);
    let first = reports.values().next().expect("reports").clone();
    for (id, report) in reports.iter() {
        assert_eq!(report, &first, "seed {seed}: replica {id} differs");
    }
    first.1
}

/// Return the workspace `target` directory.
fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

/// Write the list-append history as EDN for Elle; indeterminate and
/// unfinished operations complete as `:info`.
fn write_edn(history: &History, path: &Path) {
    let ops = history.ops.lock().unwrap();
    let events = history.events.lock().unwrap();
    let mut lines = Vec::new();
    let mut line = |kind: &str, process: u64, mops: &[Mop]| {
        let index = lines.len();
        lines.push(format!(
            "{{:type :{kind}, :f :txn, :value {}, :process {process}, :time {index}, :index {index}}}",
            edn(mops)
        ));
    };
    for &(i, completion) in events.iter() {
        let op = &ops[i];
        if !completion {
            line("invoke", op.process, &op.invoked);
            continue;
        }
        match &op.completed {
            Some((Kind::Ok, mops)) => line("ok", op.process, mops),
            Some((Kind::Fail, _)) => line("fail", op.process, &op.invoked),
            Some((Kind::Info, _)) | None => line("info", op.process, &op.invoked),
        }
    }
    for op in ops.iter().filter(|op| op.completed.is_none()) {
        line("info", op.process, &op.invoked);
    }
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Return Elle's verdict on whether the history at `path` satisfies
/// `model`, or `None` when Elle isn't available.
fn elle_verdict(path: &Path, model: &str) -> Option<bool> {
    let jar = std::env::var_os("ELLE_CLI")
        .map_or_else(|| target_dir().join("elle-cli.jar"), PathBuf::from);
    if !jar.exists() {
        eprintln!(
            "elle-cli not found; history saved at {}, Elle check skipped",
            path.display()
        );
        return None;
    }
    let output = Command::new("java")
        .arg("-jar")
        .arg(&jar)
        .args(["-m", "list-append", "-c", model, "-v", "text"])
        .arg(path)
        .output()
        .expect("failed to run java");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let verdict = stdout
        .lines()
        .find(|l| l.starts_with(&path.display().to_string()))
        .unwrap_or_else(|| panic!("Elle gave no verdict for {}:\n{stdout}", path.display()));
    Some(verdict.trim_end().ends_with("true"))
}

/// Assert the history satisfies `model`, if Elle is available.
fn elle(path: &Path, model: &str) {
    assert_ne!(
        elle_verdict(path, model),
        Some(false),
        "{} isn't {model}",
        path.display()
    );
}

#[test]
/// Assert list-append histories under partitions and crashes are
/// serializable, lose no acknowledged append, apply no unknown append twice,
/// and leave identical replicas.
fn list_append_under_faults() {
    list_append(3);
}

#[test]
/// Assert the list-append checks hold on five replicas, with minorities of
/// up to two cut off.
fn five_replicas_list_append_under_faults() {
    list_append(5);
}

/// Run the list-append simulations on `nodes` replicas and check them.
fn list_append(nodes: u64) {
    check_list_append(Workload::ListAppend, nodes, "strict-serializable");
}

#[test]
/// Assert the list-append checks hold while a new replica joins, catching
/// up from snapshots, and replaces node 1, under the same faults.
fn membership_changes_under_faults() {
    check_list_append(Workload::Membership, 3, "strict-serializable");
}

#[test]
/// Assert histories whose read-only transactions take local snapshots
/// keep each client's order and lose nothing, though they may be stale.
fn local_reads_under_faults() {
    let paths = check_list_append(Workload::LocalReads, 3, "strong-session-serializable");
    // Local snapshots can be stale, so some history should show it: this
    // shows the strict check can fail, and that fresh snapshots prevent it.
    let strict: Vec<_> = paths
        .iter()
        .filter_map(|path| elle_verdict(path, "strict-serializable"))
        .collect();
    assert!(
        strict.is_empty() || strict.contains(&false),
        "no local-read history was stale"
    );
}

/// Run list-append simulations of `workload` on `nodes` replicas, check
/// what was committed, and check the history against Elle's `model`;
/// return the histories' paths.
fn check_list_append(workload: Workload, nodes: u64, model: &str) -> Vec<PathBuf> {
    let out = target_dir().join("elle");
    std::fs::create_dir_all(&out).unwrap();
    let mut paths = Vec::new();
    for seed in seeds() {
        let history = simulate(workload, nodes, seed);
        if workload == Workload::Membership {
            assert_eq!(
                history.membership_step.load(Ordering::Acquire),
                2,
                "seed {seed}: the membership changes didn't finish"
            );
            assert!(
                history.snapshot_index.load(Ordering::Relaxed) > 0,
                "seed {seed}: no replica took a snapshot"
            );
        }
        let data = final_data(&history, seed);
        let mut seen = BTreeMap::<i64, usize>::new();
        for list in data.values() {
            for v in list {
                *seen.entry(*v).or_default() += 1;
            }
        }
        let ops = history.ops.lock().unwrap();
        let mut acknowledged = 0;
        for op in ops.iter() {
            for mop in &op.invoked {
                let Mop::Append(_, v) = mop else { continue };
                let count = seen.get(v).copied().unwrap_or(0);
                match op.completed.as_ref().map(|(k, _)| *k) {
                    Some(Kind::Ok) => {
                        acknowledged += 1;
                        assert_eq!(
                            count, 1,
                            "seed {seed}: acknowledged append {v} appears {count} times"
                        );
                    }
                    Some(Kind::Fail) => {
                        assert_eq!(count, 0, "seed {seed}: failed append {v} appears")
                    }
                    Some(Kind::Info) | None => assert!(
                        count <= 1,
                        "seed {seed}: unknown append {v} appears {count} times"
                    ),
                }
            }
        }
        assert!(acknowledged > 0, "seed {seed}: nothing committed");
        let ok = ops
            .iter()
            .filter(|o| matches!(o.completed, Some((Kind::Ok, _))))
            .count();
        let unknown = ops
            .iter()
            .filter(|o| !matches!(o.completed, Some((Kind::Ok | Kind::Fail, _))))
            .count();
        eprintln!(
            "seed {seed}: {} transactions, {ok} committed, {unknown} unknown",
            ops.len()
        );
        drop(ops);
        let name = match workload {
            Workload::LocalReads => "replicated-local-reads",
            Workload::Membership => "replicated-membership",
            _ => "replicated-list-append",
        };
        let path = out.join(format!("{name}-{nodes}-{seed}.edn"));
        write_edn(&history, &path);
        elle(&path, model);
        paths.push(path);
    }
    paths
}

#[test]
/// Assert bank transfers under partitions and crashes never change the
/// total any transaction reads or any replica holds.
fn bank_under_faults() {
    for seed in seeds() {
        let history = simulate(Workload::Bank, 3, seed);
        let total = OPENING * KEYS as i64;
        let data = final_data(&history, seed);
        let held: i64 = (0..KEYS)
            .map(|k| data.get(&k).map_or(OPENING, |v| v[0]))
            .sum();
        assert_eq!(held, total, "seed {seed}: replicas hold {held}");
        let ops = history.ops.lock().unwrap();
        let mut reads = 0;
        for op in ops.iter() {
            if let Some((Kind::Ok, mops)) = &op.completed {
                for mop in mops {
                    if let Mop::Balances(Some(balances)) = mop {
                        reads += 1;
                        assert_eq!(
                            balances.iter().sum::<i64>(),
                            total,
                            "seed {seed}: read {balances:?}"
                        );
                    }
                }
            }
        }
        let committed: BTreeSet<_> = ops
            .iter()
            .enumerate()
            .filter(|(_, o)| matches!(o.completed, Some((Kind::Ok, _))))
            .map(|(i, _)| i)
            .collect();
        assert!(
            !committed.is_empty() && reads > 0,
            "seed {seed}: workload didn't run"
        );
    }
}
