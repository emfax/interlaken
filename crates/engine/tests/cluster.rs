//! Engines as members of a replicated cluster (ADR-0037 phase 5): DDL,
//! writes and reads spread across members, and membership changes behind
//! the `admin` privilege.

#![cfg(feature = "cluster")]

mod support;

use std::time::Duration;

use interlaken_engine::{Engine, EngineError};
use interlaken_ir::interp::Datum;
use interlaken_ir::security::EntityUid;
use interlaken_net::{ClusterConfig, DEFAULT_SERVER_NAME, TlsConfig};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use support::session::{admin, options, session, table};
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
    let engine = support::session::create(dir.path()).await;
    assert!(engine.cluster_status().is_none());
    assert!(
        session(&engine, admin())
            .add_member(2, "127.0.0.1:1")
            .await
            .is_err()
    );
}
