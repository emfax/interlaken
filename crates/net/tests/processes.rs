//! Cluster members as separate processes over gRPC with mutual TLS
//! (ADR-0037 phase 5: testing layers 3 and 4).
//!
//! The parent test re-runs this binary as three child processes, each a
//! cluster member committing as fast as it can and printing every
//! acknowledged commit. Every directed link between members goes through
//! a proxy in the parent, so links can be cut: a userspace proxy needs no
//! root, unlike `tc` or `iptables`.
//!
//! - **Network faults:** members are cut off, healed, and killed with
//!   `SIGKILL` and restarted, at random.
//! - **Power loss on every node:** all members' directories are on one
//!   LazyFS mount; every member is killed at once and the mount's unsynced
//!   writes are dropped, as a power cut to the whole cluster would. This
//!   needs `LAZYFS_ROOT` (see `scripts/power-loss.sh`) and skips without it.
//!
//! Afterwards the workload stops, the members converge, and the parent
//! opens each member's directory: every member holds the same data, and
//! every commit acknowledged under `Durability::Sync` is in it.
//!
//! `NET_ROUNDS` sets the fault rounds (default 8), `NET_SEED` their seed,
//! and `POWER_CYCLES` the power-loss cycles (default 3).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::{LazyFs, Pki};
use interlaken_net::{Cluster, ClusterConfig, TlsConfig};
use interlaken_storage::{
    Durability, FjallStorage, KeyRange, ScanRequest, Storage, StorageOptions, TxnOptions,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// Members in each cluster.
const NODES: u64 = 3;
/// Committing tasks in each member.
const WORKERS: u64 = 4;

/// Print one line to stdout immediately, past the test harness's capture.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

/// Parse `1=addr,2=addr` into addresses by node.
fn parse_addresses(text: &str) -> BTreeMap<u64, String> {
    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (id, addr) = part.split_once('=').unwrap();
            (id.parse().unwrap(), addr.to_owned())
        })
        .collect()
}

/// Return the environment variable `name`, which the parent always sets.
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"))
}

#[test]
#[ignore = "run by the process tests as a child"]
/// Run one cluster member: commit forever, reporting acknowledgements,
/// until the stop file appears; then report the visible timestamp.
fn node_child() {
    let id: u64 = env("NODE_ID").parse().unwrap();
    let dir = PathBuf::from(env("NODE_DIR"));
    let certs = PathBuf::from(env("NODE_CERTS"));
    let stop = PathBuf::from(env("NODE_STOP"));
    let boot: u64 = env("NODE_BOOT").parse().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let tls = TlsConfig::new(
            std::fs::read(certs.join("ca.pem")).unwrap(),
            std::fs::read(certs.join(format!("node{id}.pem"))).unwrap(),
            std::fs::read(certs.join(format!("node{id}.key"))).unwrap(),
        );
        let mut config = ClusterConfig::new(id, env("NODE_LISTEN").parse().unwrap(), tls);
        config.peers = parse_addresses(&env("NODE_PEERS"));
        config.bootstrap = env("NODE_BOOTSTRAP") == "1";
        config.replica.heartbeat = Duration::from_millis(100);
        config.replica.election_min = Duration::from_millis(500);
        config.replica.election_max = Duration::from_millis(1000);
        config.replica.snapshot_every = 500;
        config.replica.logs_kept = 100;
        config.request_timeout = Duration::from_secs(2);
        let options = StorageOptions {
            durability: Durability::Sync,
            gc_interval: None,
            ..StorageOptions::default()
        };
        let mut cluster = None;
        for _ in 0..100 {
            match Cluster::open(dir.clone(), options.clone(), config.clone()).await {
                Ok(opened) => {
                    cluster = Some(Arc::new(opened));
                    break;
                }
                // The previous process's port or files may not be free yet.
                Err(e) => {
                    eprintln!("opening: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        let cluster = cluster.expect("the member opens");
        if config.bootstrap {
            let members = parse_addresses(&env("NODE_MEMBERS"));
            for (member, addr) in members {
                while !cluster.status().voters.contains(&member) {
                    if cluster.add_member(member, &addr).await.is_err() {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
        say("READY");
        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let cluster = Arc::clone(&cluster);
                let stop = stop.clone();
                tokio::spawn(async move {
                    for seq in 0.. {
                        if stop.exists() {
                            return;
                        }
                        // Fixed-width parts: no key may be a prefix of another,
                        // or scans can skip keys (see `Txn::put`).
                        let key = format!("n{id}/{boot:04}/{worker}/{seq:010}");
                        let committed = async {
                            let txn = cluster.storage().begin(TxnOptions::default()).await?;
                            txn.put(Bytes::from(key.clone()), Bytes::from_static(b"1"))?;
                            txn.commit().await
                        };
                        match committed.await {
                            Ok(info) => say(&format!(
                                "ACK {key} {}",
                                info.commit_ts.map_or(0, |ts| ts.into_raw())
                            )),
                            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            let _ = worker.await;
        }
        loop {
            say(&format!(
                "STATE {}",
                cluster.storage().visible_ts().into_raw()
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
}

/// A TCP proxy for one directed link, which can be cut.
struct Proxy {
    /// The port it listens on.
    port: u16,
    /// Whether the link is up; cutting it drops every connection.
    up: watch::Sender<bool>,
}

impl Proxy {
    /// Start a proxy forwarding to `target`'s port on localhost.
    async fn start(target: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (up, watched) = watch::channel(true);
        tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                if !*watched.borrow() {
                    continue;
                }
                let mut watched = watched.clone();
                tokio::spawn(async move {
                    let Ok(outbound) = TcpStream::connect(("127.0.0.1", target)).await else {
                        return;
                    };
                    let (mut a, mut b) = (inbound, outbound);
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {}
                        _ = watched.wait_for(|up| !up) => {}
                    }
                });
            }
        });
        Self { port, up }
    }
}

/// What the children have reported.
#[derive(Default)]
struct Reports {
    /// Keys whose commits were acknowledged.
    acked: BTreeSet<String>,
    /// Each acknowledged key's commit timestamp.
    commit_ts: BTreeMap<String, u64>,
    /// Each member's latest visible timestamp, once stopped.
    states: BTreeMap<u64, u64>,
    /// Members that have reported ready, by boot.
    ready: BTreeSet<u64>,
}

/// Three member processes and the proxies between them.
struct Harness {
    /// Where member directories live.
    root: PathBuf,
    /// The certificates.
    certs: PathBuf,
    /// The file whose appearance stops the workload.
    stop: PathBuf,
    /// Each member's listening port.
    listen: BTreeMap<u64, u16>,
    /// The proxy for each directed link, by (from, to).
    proxies: BTreeMap<(u64, u64), Proxy>,
    /// The running members.
    children: BTreeMap<u64, Child>,
    /// What the members reported.
    reports: Arc<Mutex<Reports>>,
    /// The next boot number.
    boot: u64,
    /// Holds the certificate and stop-file directory.
    _work: tempfile::TempDir,
}

/// Return a free localhost port.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Harness {
    /// Prepare certificates, ports and proxies for members under `root`.
    async fn new(root: &Path) -> Self {
        let work = tempfile::tempdir().unwrap();
        let certs = work.path().join("certs");
        std::fs::create_dir(&certs).unwrap();
        let pki = Pki::new();
        for id in 1..=NODES {
            let tls = pki.node();
            std::fs::write(certs.join("ca.pem"), &tls.ca).unwrap();
            std::fs::write(certs.join(format!("node{id}.pem")), &tls.cert).unwrap();
            std::fs::write(certs.join(format!("node{id}.key")), &tls.key).unwrap();
        }
        let listen: BTreeMap<u64, u16> = (1..=NODES).map(|id| (id, free_port())).collect();
        let mut proxies = BTreeMap::new();
        for from in 1..=NODES {
            for to in (1..=NODES).filter(|&to| to != from) {
                proxies.insert((from, to), Proxy::start(listen[&to]).await);
            }
        }
        Self {
            root: root.to_path_buf(),
            certs,
            stop: work.path().join("stop"),
            listen,
            proxies,
            children: BTreeMap::new(),
            reports: Arc::new(Mutex::new(Reports::default())),
            boot: 0,
            _work: work,
        }
    }

    /// Start member `id`; member 1 bootstraps the cluster and adds the others.
    fn spawn(&mut self, id: u64) {
        self.boot += 1;
        let dir = self.root.join(format!("node{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let peers: Vec<String> = (1..=NODES)
            .filter(|&p| p != id)
            .map(|p| format!("{p}=127.0.0.1:{}", self.proxies[&(id, p)].port))
            .collect();
        let members: Vec<String> = (2..=NODES)
            .map(|m| format!("{m}=127.0.0.1:{}", self.listen[&m]))
            .collect();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "node_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("NODE_ID", id.to_string())
            .env("NODE_DIR", &dir)
            .env("NODE_CERTS", &self.certs)
            .env("NODE_STOP", &self.stop)
            .env("NODE_BOOT", self.boot.to_string())
            .env("NODE_LISTEN", format!("127.0.0.1:{}", self.listen[&id]))
            .env("NODE_PEERS", peers.join(","))
            .env("NODE_MEMBERS", members.join(","))
            .env("NODE_BOOTSTRAP", if id == 1 { "1" } else { "0" })
            .stdout(Stdio::piped())
            .stderr(
                std::fs::File::create(
                    self.stop
                        .with_file_name(format!("node{id}-{}.log", self.boot)),
                )
                .unwrap(),
            )
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let reports = Arc::clone(&self.reports);
        let boot = self.boot;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let mut reports = reports.lock().unwrap();
                if let Some((_, ack)) = line.split_once("ACK ") {
                    let mut parts = ack.split_whitespace();
                    let key = parts.next().unwrap().to_owned();
                    let ts: u64 = parts.next().map_or(0, |t| t.parse().unwrap_or(0));
                    reports.commit_ts.insert(key.clone(), ts);
                    reports.acked.insert(key);
                } else if let Some((_, ts)) = line.split_once("STATE ") {
                    reports.states.insert(id, ts.trim().parse().unwrap());
                } else if line.contains("READY") {
                    reports.ready.insert(boot);
                }
            }
        });
        self.children.insert(id, child);
    }

    /// Kill member `id` with `SIGKILL`.
    fn kill(&mut self, id: u64) {
        if let Some(mut child) = self.children.remove(&id) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// Cut, or restore, every link to and from member `id`.
    fn set_links(&self, id: u64, up: bool) {
        for ((from, to), proxy) in &self.proxies {
            if *from == id || *to == id {
                proxy.up.send_replace(up);
            }
        }
    }

    /// Restore every link.
    fn heal(&self) {
        for proxy in self.proxies.values() {
            proxy.up.send_replace(true);
        }
    }

    /// Return how many commits have been acknowledged.
    fn acked(&self) -> usize {
        self.reports.lock().unwrap().acked.len()
    }

    /// Stop the workload, wait until the members agree on a visible
    /// timestamp, and kill them.
    async fn converge(&mut self) {
        std::fs::write(&self.stop, b"").unwrap();
        let mut agreed = 0;
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let states = self.reports.lock().unwrap().states.clone();
            let same = states.len() as u64 == NODES
                && states.values().all(|ts| Some(ts) == states.values().next());
            agreed = if same { agreed + 1 } else { 0 };
            if agreed >= 8 {
                for id in 1..=NODES {
                    self.kill(id);
                }
                return;
            }
        }
        let logs = self.logs();
        let alive: Vec<_> = self
            .children
            .iter_mut()
            .map(|(id, child)| (*id, child.try_wait().unwrap().is_none()))
            .collect();
        panic!(
            "members never converged: {:?}, alive {alive:?}\n{logs}",
            self.reports.lock().unwrap().states
        );
    }

    /// Open every member's directory and assert they hold the same keys,
    /// including every acknowledged one.
    async fn verify(&self) {
        let (acked, commit_ts) = {
            let reports = self.reports.lock().unwrap();
            (reports.acked.clone(), reports.commit_ts.clone())
        };
        let mut members = BTreeMap::new();
        for id in 1..=NODES {
            let options = StorageOptions {
                gc_interval: None,
                ..StorageOptions::default()
            };
            let storage = FjallStorage::open(self.root.join(format!("node{id}")), options).unwrap();
            let txn = storage
                .begin(TxnOptions::default().read_only())
                .await
                .unwrap();
            let mut keys = BTreeSet::new();
            let mut request = ScanRequest::new(KeyRange::prefix(b"n"));
            loop {
                let batch = txn.scan(request.clone()).await.unwrap();
                keys.extend(
                    batch
                        .rows
                        .iter()
                        .map(|(k, _)| String::from_utf8(k.to_vec()).unwrap()),
                );
                match request.resume_after(&batch) {
                    Some(next) => request = next,
                    None => break,
                }
            }
            members.insert(id, (storage.visible_ts().into_raw(), keys));
        }
        let mut problems = Vec::new();
        for (id, (visible, keys)) in &members {
            let lost: Vec<_> = acked.difference(keys).collect();
            if !lost.is_empty() {
                let examples: Vec<_> = lost
                    .iter()
                    .take(5)
                    .map(|k| format!("{k} at {}", commit_ts[*k]))
                    .collect();
                problems.push(format!(
                    "member {id} (visible {visible}, {} keys) lost {} acknowledged commits, such as {examples:?}",
                    keys.len(),
                    lost.len()
                ));
            }
        }
        let first = &members[&1].1;
        for (id, (_, keys)) in &members {
            if keys != first {
                problems.push(format!("member {id} differs from member 1"));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        assert!(!acked.is_empty(), "nothing was acknowledged");
        eprintln!("{} acknowledged commits, all on every member", acked.len());
    }

    /// Wait until `count` boots have reported ready.
    async fn await_ready(&self, count: usize) {
        for _ in 0..600 {
            if self.reports.lock().unwrap().ready.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("members didn't start\n{}", self.logs());
    }

    /// Return the last lines each member process wrote to stderr.
    fn logs(&self) -> String {
        let mut logs = String::new();
        for entry in std::fs::read_dir(self.stop.parent().unwrap()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "log") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(15).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                logs.push_str(&format!("== {}\n{}\n", path.display(), tail.join("\n")));
            }
        }
        logs
    }
}

impl Drop for Harness {
    /// Kill any members still running.
    fn drop(&mut self) {
        for child in self.children.values_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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

/// Mount LazyFS and check it works, remounting if its FUSE process died
/// at startup, which happens now and then.
fn usable_lazyfs() -> LazyFs {
    for _ in 0..3 {
        let lazyfs = LazyFs::from_env().expect("LAZYFS_ROOT is set");
        let probe = lazyfs.path().join("probe");
        if std::fs::write(&probe, b"ok").is_ok() && std::fs::read(&probe).is_ok_and(|b| b == b"ok")
        {
            return lazyfs;
        }
        eprintln!("LazyFS mounted but isn't usable; remounting");
    }
    panic!("LazyFS never mounted usably");
}

/// Return the number in environment variable `name`, or `default`.
fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert members cut off, healed, killed and restarted at random lose no
/// acknowledged commit and converge to the same data.
async fn network_faults_lose_no_acknowledged_commit() {
    let root = tempfile::tempdir().unwrap();
    let mut harness = Harness::new(root.path()).await;
    for id in 1..=NODES {
        harness.spawn(id);
    }
    harness.await_ready(NODES as usize).await;
    let seed = setting("NET_SEED", 1);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut down = None;
    let mut log = Vec::new();
    for _ in 0..setting("NET_ROUNDS", 8) {
        if let Some(id) = down.take() {
            harness.spawn(id);
            log.push(format!("restart {id}"));
        }
        match rng.next() % 3 {
            0 => {
                let id = 1 + rng.next() % NODES;
                harness.heal();
                harness.set_links(id, false);
                log.push(format!("cut {id}"));
            }
            1 => {
                harness.heal();
                log.push("heal".into());
            }
            _ => {
                let id = 1 + rng.next() % NODES;
                harness.kill(id);
                down = Some(id);
                log.push(format!("kill {id}"));
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    harness.heal();
    if let Some(id) = down {
        harness.spawn(id);
    }
    eprintln!(
        "seed {seed}: {log:?}; {} acknowledged before stopping",
        harness.acked()
    );
    harness.converge().await;
    harness.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert power loss on every member at once loses no commit acknowledged
/// under `Sync`, and the members converge afterwards (needs `LAZYFS_ROOT`).
async fn power_loss_on_every_member_loses_no_acknowledged_commit() {
    if std::env::var_os("LAZYFS_ROOT").is_none() {
        eprintln!(
            "skipping the cluster power-loss test: set LAZYFS_ROOT to a built LazyFS checkout"
        );
        return;
    }
    let lazyfs = usable_lazyfs();
    let mut harness = Harness::new(lazyfs.path()).await;
    for id in 1..=NODES {
        harness.spawn(id);
    }
    harness.await_ready(NODES as usize).await;
    let cycles = setting("POWER_CYCLES", 3);
    for cycle in 0..cycles {
        tokio::time::sleep(Duration::from_secs(3)).await;
        for id in 1..=NODES {
            harness.kill(id);
        }
        lazyfs.clear_cache();
        eprintln!(
            "power loss {cycle}: {} acknowledged so far",
            harness.acked()
        );
        let boots = harness.boot as usize;
        for id in 1..=NODES {
            harness.spawn(id);
        }
        harness.await_ready(boots + NODES as usize).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    harness.converge().await;
    harness.verify().await;
}
