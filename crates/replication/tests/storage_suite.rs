//! openraft's storage conformance suite, run against the log store and state machine.
//!
//! The suite's tests that build or install snapshots are left out until
//! snapshots arrive in phase 4: `snapshot_meta`, `snapshot_meta_optional`,
//! `transfer_snapshot`, and the `get_initial_state_*` tests whose state
//! machine starts ahead of the log (`membership_from_empty_log_and_sm`,
//! `membership_from_sm_inlog_is_smaller`, `membership_from_log_insm_is_smaller`,
//! `last_log_lt_sm`, `last_log_lt_sm_past_snapshot`, `sm_lt_purged`,
//! `log_ids` and `re_apply_committed`).

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

/// Run each named suite test on a fresh store, failing with its name.
macro_rules! run {
    ($($test:ident),* $(,)?) => {
        $(
            let (_g, store, sm) = Builder.build().await.unwrap();
            if let Err(e) = S::$test(store, sm).await {
                panic!("{}: {e}", stringify!($test));
            }
        )*
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// Assert the log store and state machine pass openraft's storage suite.
async fn openraft_storage_suite() {
    run!(
        last_membership_in_log_initial,
        last_membership_in_log,
        last_membership_in_log_multi_step,
        get_membership_initial,
        get_membership_from_log_and_empty_sm,
        get_membership_from_empty_log_and_sm,
        get_membership_from_log_le_sm_last_applied,
        get_membership_from_log_gt_sm_last_applied_1,
        get_membership_from_log_gt_sm_last_applied_2,
        get_initial_state_without_init,
        get_initial_state_with_state,
        get_initial_state_last_log_gt_sm,
        get_initial_state_inverted_log_order,
        get_initial_state_conflict_at_same_index,
        save_vote,
        get_log_entries,
        limited_get_log_entries,
        leader_bounded_stream,
        entries_stream,
        try_get_log_entry,
        log_reader_reads_new_entries,
        initial_logs,
        get_log_state,
        get_log_id,
        last_id_in_log,
        last_applied_state,
        purge_logs_upto_0,
        purge_logs_upto_5,
        purge_logs_upto_20,
        delete_logs_after_11,
        delete_logs_after_5,
        delete_logs_after_0,
        delete_logs_after_none,
        append_to_log,
        apply_single,
        apply_multiple,
    );
}
