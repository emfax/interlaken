//! Engines as members of a replicated cluster (ADR-0037 phases 5 and 6,
//! testing layer 6): DDL, writes and reads spread across members,
//! membership changes behind the `admin` privilege, policies and their
//! caches across members, provers on followers, and bootstrapping from a
//! single-node database.

#![cfg(feature = "cluster")]

mod support;

use std::time::Duration;

use std::sync::Arc;

use interlaken_engine::catalog::{AttributeSpec, EntitySpec, load_catalog};
use interlaken_engine::policy::{
    Gate, GateResult, PolicyDocument, PolicyLanguage, PolicySettings, Proposal, Prover,
    VerificationMode, watch_proposals,
};
use interlaken_engine::{Engine, EngineError, Session};
use interlaken_ir::catalog::Catalog;
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use interlaken_net::{ClusterConfig, DEFAULT_SERVER_NAME, TlsConfig};
use interlaken_storage::{Storage, TxnOptions};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use support::session::{activate_rls, admin, options, replace_rows, session, table};
use tempfile::TempDir;

/// A cluster CA and the node certificates it signs.
struct Pki {
    /// The CA's certificate, PEM-encoded.
    ca: String,
    /// The CA, for signing.
    issuer: Issuer<'static, KeyPair>,
}

impl Pki {
    /// Construct a new CA.
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap().pem();
        Self {
            ca,
            issuer: Issuer::new(params, key),
        }
    }

    /// Return a node's TLS configuration.
    fn node(&self) -> TlsConfig {
        let key = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec![DEFAULT_SERVER_NAME.to_owned()]).unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        TlsConfig::new(
            self.ca.clone().into_bytes(),
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
        )
    }
}

/// Open node `id` in `dir` as a cluster member, bootstrapping if `bootstrap`.
async fn member(id: u64, dir: &TempDir, pki: &Pki, bootstrap: bool) -> Engine {
    let mut config = ClusterConfig::new(id, "127.0.0.1:0".parse().unwrap(), pki.node());
    config.bootstrap = bootstrap;
    config.replica.heartbeat = Duration::from_millis(100);
    config.replica.election_min = Duration::from_millis(500);
    config.replica.election_max = Duration::from_millis(1000);
    let admin = admin();
    Engine::open_cluster(dir.path(), options(), config, bootstrap.then_some(&admin))
        .await
        .unwrap()
}

/// Return the address node `engine` serves its peers on.
fn addr(engine: &Engine) -> String {
    engine.cluster().unwrap().local_addr().to_string()
}

/// Return the SQLSTATE of a failed result.
fn state<T: std::fmt::Debug>(result: Result<T, EngineError>) -> &'static str {
    result.unwrap_err().sqlstate()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert DDL on one member, writes on another and reads on a third all
/// agree, and that only administrators change membership.
async fn members_share_one_database() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let first = member(1, &dirs[0], &pki, true).await;
    let second = member(2, &dirs[1], &pki, false).await;
    let third = member(3, &dirs[2], &pki, false).await;

    let outsider = EntityUid::long("User", 7);
    assert_eq!(
        state(
            session(&first, outsider)
                .add_member(2, &addr(&second))
                .await
        ),
        "42501"
    );
    let mut root = session(&first, admin());
    root.add_member(2, &addr(&second)).await.unwrap();
    root.add_member(3, &addr(&third)).await.unwrap();
    assert_eq!(first.cluster_status().unwrap().voters, vec![1, 2, 3]);

    session(&second, admin())
        .create_table(&table("t", &[("id", "int8"), ("n", "int8")], &["id"]))
        .await
        .unwrap();
    session(&third, admin())
        .query(
            "(plan (insert t (values (columns a b) (row 1 10) (row 2 20)) (columns id n)))",
            Vec::new(),
        )
        .await
        .unwrap();
    let rows = root
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .rows();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&vec![Datum::Int8(2), Datum::Int8(20)]));

    let leader = first.cluster_status().unwrap().leader.unwrap();
    let target = [1, 2, 3].into_iter().find(|&id| id != leader).unwrap();
    root.transfer_leadership(target).await.unwrap();
    for _ in 0..200 {
        if first.cluster_status().unwrap().leader == Some(target) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(first.cluster_status().unwrap().leader, Some(target));
    let rows = session(&second, admin())
        .query("(plan (get t))", Vec::new())
        .await
        .unwrap()
        .rows();
    assert_eq!(rows.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert a single-node database reports no cluster, and refuses membership changes.
async fn single_nodes_have_no_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let engine = support::session::create_single(dir.path()).await;
    assert!(engine.cluster_status().is_none());
    assert!(
        session(&engine, admin())
            .add_member(2, "127.0.0.1:1")
            .await
            .is_err()
    );
}

/// Form a cluster of three in `dirs`: node 1 bootstraps a new database
/// and adds the others.
async fn three(pki: &Pki, dirs: &[TempDir; 3]) -> [Engine; 3] {
    let first = member(1, &dirs[0], pki, true).await;
    let second = member(2, &dirs[1], pki, false).await;
    let third = member(3, &dirs[2], pki, false).await;
    let mut root = session(&first, admin());
    root.add_member(2, &addr(&second)).await.unwrap();
    root.add_member(3, &addr(&third)).await.unwrap();
    [first, second, third]
}

/// Return user `n`.
fn user(n: i64) -> EntityUid {
    EntityUid::long("User", n)
}

/// Return the document IDs `principal` sees on `engine`, sorted.
async fn visible_docs(engine: &Engine, principal: EntityUid) -> Vec<i64> {
    let mut ids: Vec<i64> = session(engine, principal)
        .query("(plan (project (get docs :as d) d.id))", Vec::new())
        .await
        .unwrap()
        .rows()
        .into_iter()
        .map(|row| match row[0] {
            Datum::Int8(id) => id,
            ref other => panic!("unexpected {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Create users 1 and 2 and documents 10 (owned by 1) and 11 (owned by 2)
/// through `engine`, governed by owner-only policies.
async fn govern(engine: &Engine) -> Session {
    let mut root = session(engine, admin());
    root.create_table(&table("users", &[("id", "int8")], &["id"]))
        .await
        .unwrap();
    root.create_table(&table(
        "docs",
        &[("id", "int8"), ("owner_id", "int8")],
        &["id"],
    ))
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "User".into(),
        table: "users".into(),
        key: "id".into(),
        attributes: Vec::new(),
    })
    .await
    .unwrap();
    root.declare_entity(&EntitySpec {
        name: "Doc".into(),
        table: "docs".into(),
        key: "id".into(),
        attributes: vec![AttributeSpec::reference("owner", "owner_id", "User")],
    })
    .await
    .unwrap();
    replace_rows(
        engine,
        &[
            ("users", vec![vec![Datum::Int8(1)], vec![Datum::Int8(2)]]),
            (
                "docs",
                vec![
                    vec![Datum::Int8(10), Datum::Int8(1)],
                    vec![Datum::Int8(11), Datum::Int8(2)],
                ],
            ),
        ],
    )
    .await;
    activate_rls(
        &mut root,
        "(policies
           (permit (read insert update delete) Doc (== resource.owner principal))
           (permit (read) User true))",
    )
    .await
    .unwrap();
    root
}

/// Owners may do anything with their documents; everyone reads every document.
const EVERYONE_READS: &str = "(policies
   (permit (read insert update delete) Doc (== resource.owner principal))
   (permit (read) Doc true)
   (permit (read) User true))";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a policy activated through one member governs queries on the
/// others at once, their residual caches invalidated.
async fn policies_activated_on_one_member_govern_all() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let [first, second, third] = three(&pki, &dirs).await;
    govern(&first).await;
    // Warm the residual caches under the owner-only policies.
    assert_eq!(visible_docs(&third, user(1)).await, [10]);
    assert_eq!(visible_docs(&second, user(1)).await, [10]);
    activate_rls(&mut session(&second, admin()), EVERYONE_READS)
        .await
        .unwrap();
    assert_eq!(visible_docs(&third, user(1)).await, [10, 11]);
    assert_eq!(visible_docs(&first, user(2)).await, [10, 11]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a write authorized under policies another member then replaces
/// fails at commit, as it does on one node (ADR-0019).
async fn a_policy_change_elsewhere_conflicts_with_writers() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let [first, _second, third] = three(&pki, &dirs).await;
    govern(&first).await;
    let mut writer = session(&third, user(1));
    writer.begin(TxnOptions::default()).await.unwrap();
    writer
        .query(
            "(plan (insert docs (values (columns a b) (row 12 1)) (columns id owner_id)))",
            Vec::new(),
        )
        .await
        .unwrap();
    activate_rls(&mut session(&first, admin()), EVERYONE_READS)
        .await
        .unwrap();
    assert_eq!(state(writer.commit().await), "40001");
    assert_eq!(visible_docs(&first, user(1)).await, [10, 11]);
}

/// A prover that passes every proof gate.
struct PassingProver;

impl Prover for PassingProver {
    /// Return the prover's name.
    fn name(&self) -> &str {
        "follower-prover"
    }

    /// Pass every proof gate.
    async fn prove(
        &self,
        _current: &PolicyDocument,
        _proposal: &Proposal,
        _catalog: &Arc<dyn Catalog>,
        _app_context: &str,
    ) -> Result<Vec<GateResult>, EngineError> {
        Ok(Gate::ALL
            .iter()
            .filter(|g| g.is_proof())
            .map(|g| GateResult::new(*g, true, "proved"))
            .collect())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a prover on a follower proves a proposal submitted on another
/// member, so that a third can activate it in proof-required mode.
async fn provers_on_followers_prove_replicated_proposals() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let [first, second, third] = three(&pki, &dirs).await;
    govern(&first).await;
    session(&first, admin())
        .set_policy_settings(&PolicySettings {
            mode: VerificationMode::ProofRequired,
            app_context: "{}".into(),
        })
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let prover = tokio::spawn(async move {
        watch_proposals(&third, &PassingProver, Duration::from_millis(50), async {
            let _ = stopped.await;
        })
        .await
    });
    let mut proposer = session(&first, admin());
    let document = PolicyDocument {
        language: PolicyLanguage::Rls,
        body: EVERYONE_READS.into(),
    };
    let id = proposer.submit_policies(&document, "").await.unwrap();
    proposer.verify_policies(id).await.unwrap();
    let mut activator = session(&second, admin());
    let mut activated = false;
    for _ in 0..200 {
        match activator.activate_policies(id, true).await {
            Ok(_) => {
                activated = true;
                break;
            }
            Err(EngineError::AwaitingProof(_)) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(activated, "the follower's prover never proved the proposal");
    assert_eq!(visible_docs(&second, user(1)).await, [10, 11]);
    stop.send(()).unwrap();
    prover.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a cluster bootstrapped from a single-node database keeps its
/// data and its schema version, on every member.
async fn a_cluster_keeps_a_single_node_database() {
    let pki = Pki::new();
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    let version = {
        let engine = support::session::create_single(dirs[0].path()).await;
        let mut root = session(&engine, admin());
        root.create_table(&table("t", &[("id", "int8"), ("n", "int8")], &["id"]))
            .await
            .unwrap();
        root.query(
            "(plan (insert t (values (columns a b) (row 1 10)) (columns id n)))",
            Vec::new(),
        )
        .await
        .unwrap();
        let txn = engine
            .storage()
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        load_catalog(&txn).await.unwrap().schema_version()
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let first = member(1, &dirs[0], &pki, true).await;
    let second = member(2, &dirs[1], &pki, false).await;
    let third = member(3, &dirs[2], &pki, false).await;
    let mut root = session(&first, admin());
    root.add_member(2, &addr(&second)).await.unwrap();
    root.add_member(3, &addr(&third)).await.unwrap();
    for engine in [&first, &second, &third] {
        let rows = session(engine, admin())
            .query("(plan (get t))", Vec::new())
            .await
            .unwrap()
            .rows();
        assert_eq!(rows, vec![vec![Datum::Int8(1), Datum::Int8(10)]]);
        let txn = engine
            .storage()
            .begin(TxnOptions::default().read_only())
            .await
            .unwrap();
        assert_eq!(load_catalog(&txn).await.unwrap().schema_version(), version);
    }
}
