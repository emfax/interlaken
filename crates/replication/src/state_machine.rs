//! The replicated state machine: the replica's storage (ADR-0037).
//!
//! Applying a committed entry writes its commit group's versions through
//! [`Replica::apply`], with the entry's log ID, and its membership if it
//! changes membership, recorded in `meta` in the same batch. A restarted
//! replica therefore knows exactly which entries its data already holds.
//! Applied indexes are published on a watch channel, which the commit
//! path waits on so a transaction's node answers only once it has applied
//! the commit itself.
//!
//! Snapshots come with phase 4; until then the state machine refuses to
//! build or install one, and replicas catch up from the log.

use futures::StreamExt;
use interlaken_storage::replica::Replica;
use openraft::EntryPayload;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use tokio::sync::watch;

use crate::types::{LogId, StoredMembership, TypeConfig, decode, encode, io};

/// The `meta` key of the last applied log ID.
const APPLIED: &[u8] = b"raft_applied";
/// The `meta` key of the last applied membership.
const MEMBERSHIP: &[u8] = b"raft_membership";

/// Snapshot data: not built until phase 4.
pub type SnapshotData = std::io::Cursor<Vec<u8>>;

/// A replica's state machine.
#[derive(Debug)]
pub struct StateMachine {
    /// The replica's storage.
    replica: Replica,
    /// Where applied indexes are published.
    applied: watch::Sender<u64>,
}

impl StateMachine {
    /// Apply to `replica`, publishing applied indexes on `applied`.
    #[must_use]
    pub fn new(replica: Replica, applied: watch::Sender<u64>) -> Self {
        Self { replica, applied }
    }
}

/// Builds snapshots; refuses until phase 4.
#[derive(Debug)]
pub struct SnapshotBuilder;

/// The error for snapshot operations before phase 4.
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "snapshots are not supported yet (ADR-0037 phase 4)",
    )
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    type SnapshotData = SnapshotData;

    /// Refuse: snapshots come with phase 4.
    async fn build_snapshot(
        &mut self,
    ) -> Result<openraft::alias::SnapshotOf<TypeConfig, SnapshotData>, std::io::Error> {
        Err(unsupported())
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotData = SnapshotData;
    type SnapshotBuilder = SnapshotBuilder;

    /// Return the last applied log ID and membership.
    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), std::io::Error> {
        let applied: Option<LogId> = match self.replica.meta(APPLIED.to_vec()).await.map_err(io)? {
            Some(bytes) => decode(&bytes)?,
            None => None,
        };
        self.applied
            .send_replace(applied.as_ref().map_or(0, openraft::LogId::index));
        let membership = match self.replica.meta(MEMBERSHIP.to_vec()).await.map_err(io)? {
            Some(bytes) => decode(&bytes)?,
            None => StoredMembership::default(),
        };
        Ok((applied, membership))
    }

    /// Apply each committed entry in order, answering its responder.
    async fn apply<S>(&mut self, mut entries: S) -> Result<(), std::io::Error>
    where
        S: futures::Stream<Item = Result<EntryResponder<TypeConfig>, std::io::Error>>
            + Unpin
            + Send,
    {
        while let Some(item) = entries.next().await {
            let (entry, responder) = item?;
            let log_id = entry.log_id;
            let mut meta = vec![(APPLIED.to_vec(), encode(&Some(log_id))?)];
            let batch = match entry.payload {
                EntryPayload::Blank => None,
                EntryPayload::Normal(batch) => Some(bytes::Bytes::from(batch.0)),
                EntryPayload::Membership(membership) => {
                    let stored = StoredMembership::new(Some(log_id), membership);
                    meta.push((MEMBERSHIP.to_vec(), encode(&stored)?));
                    None
                }
            };
            self.replica.apply(batch, meta).await.map_err(io)?;
            self.applied.send_replace(log_id.index());
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }

    /// Return a snapshot builder, which refuses until phase 4.
    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder
    }

    /// Refuse: snapshots come with phase 4.
    async fn install_snapshot(
        &mut self,
        _meta: &openraft::alias::SnapshotMetaOf<TypeConfig>,
        _snapshot: SnapshotData,
    ) -> Result<(), std::io::Error> {
        Err(unsupported())
    }

    /// Return no snapshot: none is built before phase 4.
    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<openraft::alias::SnapshotOf<TypeConfig, SnapshotData>>, std::io::Error> {
        Ok(None)
    }
}
