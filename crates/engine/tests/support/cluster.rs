//! Running engine tests through a three-node cluster (ADR-0037, testing layer 5).
//!
//! With the `cluster` feature and `INTERLAKEN_TEST_CLUSTER` set,
//! [`create`](super::session::create) builds a cluster of three members
//! in the test's directory and returns a follower, so every statement
//! commits through the leader and reads through a read index. Results must
//! match a single node's.

use std::sync::Mutex;
use std::time::Duration;

use interlaken_engine::Engine;
use interlaken_net::{ClusterConfig, DEFAULT_SERVER_NAME, TlsConfig};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

use super::session::{admin, options, session};

/// Members kept running for the rest of the test process.
static MEMBERS: Mutex<Vec<Engine>> = Mutex::new(Vec::new());

/// Return TLS configurations for `count` nodes, signed by a new CA.
fn certificates(count: usize) -> Vec<TlsConfig> {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = params.self_signed(&key).unwrap().pem();
    let issuer = Issuer::new(params, key);
    (0..count)
        .map(|_| {
            let key = KeyPair::generate().unwrap();
            let params = CertificateParams::new(vec![DEFAULT_SERVER_NAME.to_owned()]).unwrap();
            let cert = params.signed_by(&key, &issuer).unwrap();
            TlsConfig::new(
                ca.clone().into_bytes(),
                cert.pem().into_bytes(),
                key.serialize_pem().into_bytes(),
            )
        })
        .collect()
}

/// Open member `id` in `dir`; member 1 bootstraps a new database.
async fn member(id: u64, dir: &std::path::Path, tls: TlsConfig) -> Engine {
    let mut config = ClusterConfig::new(id, "127.0.0.1:0".parse().unwrap(), tls);
    config.bootstrap = id == 1;
    config.replica.heartbeat = Duration::from_millis(100);
    config.replica.election_min = Duration::from_millis(500);
    config.replica.election_max = Duration::from_millis(1000);
    let admin = admin();
    std::fs::create_dir_all(dir).unwrap();
    Engine::open_cluster(dir, options(), config, (id == 1).then_some(&admin))
        .await
        .unwrap()
}

/// Create a database on a new three-member cluster under `dir` and return
/// a follower, keeping the other members running.
pub async fn create(dir: &std::path::Path) -> Engine {
    let mut tls = certificates(3).into_iter();
    let first = member(1, &dir.join("node1"), tls.next().unwrap()).await;
    let second = member(2, &dir.join("node2"), tls.next().unwrap()).await;
    let third = member(3, &dir.join("node3"), tls.next().unwrap()).await;
    let mut root = session(&first, admin());
    for (id, engine) in [(2, &second), (3, &third)] {
        let addr = engine.cluster().unwrap().local_addr().to_string();
        root.add_member(id, &addr).await.unwrap();
    }
    MEMBERS.lock().unwrap().extend([first, second]);
    third
}
