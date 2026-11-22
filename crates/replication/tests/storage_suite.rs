//! openraft's storage conformance suite, run against the log store and state machine.

use std::sync::Arc;

use interlaken_replication::log_store::LogStore;
use interlaken_replication::state_machine::StateMachine;
use interlaken_replication::types::TypeConfig;
use interlaken_storage::replica::{CommitFuture, CommitPath};
use interlaken_storage::{CommitRequest, Error, FjallStorage, StorageOptions};
use openraft::StorageError;
use openraft::testing::log::{StoreBuilder, Suite};
use tempfile::TempDir;
use tokio::sync::watch;

/// A commit path that refuses everything: the suite never commits.
struct NoCommits;

impl CommitPath for NoCommits {
    /// Refuse the commit.
    fn submit(&self, _request: CommitRequest) -> CommitFuture<'_> {
        Box::pin(async {
            Err(Error::ReplicationUnavailable(
                "no commits in the suite".into(),
            ))
        })
    }
}

/// Builds a fresh store in a temporary directory for each suite test.
struct Builder;

impl StoreBuilder<TypeConfig, LogStore, StateMachine, (TempDir, FjallStorage)> for Builder {
    /// Open an empty replica and return its log store and state machine.
    async fn build(
        &self,
    ) -> Result<((TempDir, FjallStorage), LogStore, StateMachine), StorageError<TypeConfig>> {
        let dir = tempfile::tempdir().unwrap();
        let mut replica = None;
        let storage = FjallStorage::open_replicated(dir.path(), &StorageOptions::default(), |r| {
            replica = Some(r);
            Ok(Arc::new(NoCommits) as Arc<dyn CommitPath>)
        })
        .unwrap();
        let replica = replica.unwrap();
        let (applied, _) = watch::channel(0);
        Ok((
            (dir, storage),
            LogStore::new(replica.clone(), true),
            StateMachine::new(replica, applied),
        ))
    }
}

/// The suite, for our store.
type S = Suite<TypeConfig, LogStore, StateMachine, Builder, (TempDir, FjallStorage)>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert the log store and state machine pass openraft's whole storage suite.
async fn openraft_storage_suite() {
    S::test_all(Builder).await.unwrap();
}
