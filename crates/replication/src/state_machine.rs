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
//! **Snapshots** are the replica's whole `data` keyspace with its applied
//! log ID and membership, captured between applies and exported by storage
//! ([`Replica::capture`]). Every built or received snapshot is stored
//! durably, with its Raft metadata, before anything relies on it: the
//! leader sends the stored one to replicas too far behind its log, and a
//! replica installing one redoes the import from it if a crash interrupts.
//! A snapshot travels and is held in memory whole; streaming it is future
//! work.

use std::io::Cursor;

use futures::StreamExt;
use interlaken_storage::replica::{Replica, SnapshotView};
use openraft::EntryPayload;
use openraft::alias::{SnapshotMetaOf, SnapshotOf};
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use tokio::sync::watch;

use crate::types::{LogId, StoredMembership, TypeConfig, decode, encode, io};

/// The `meta` key of the last applied log ID.
const APPLIED: &[u8] = b"raft_applied";
/// The `meta` key of the last applied membership.
const MEMBERSHIP: &[u8] = b"raft_membership";

/// Snapshot data: storage's export, whole.
pub type SnapshotData = Cursor<Vec<u8>>;

/// A snapshot with its Raft metadata.
type Snapshot = SnapshotOf<TypeConfig, SnapshotData>;
/// A snapshot's Raft metadata.
type SnapshotMeta = SnapshotMetaOf<TypeConfig>;

/// Encode a snapshot for storing: its metadata's length, the metadata, then the data.
fn wrap(meta: &SnapshotMeta, data: &[u8]) -> std::io::Result<Vec<u8>> {
    let header = encode(meta)?;
    let mut blob = Vec::with_capacity(4 + header.len() + data.len());
    blob.extend_from_slice(
        &u32::try_from(header.len())
            .map_err(std::io::Error::other)?
            .to_be_bytes(),
    );
    blob.extend_from_slice(&header);
    blob.extend_from_slice(data);
    Ok(blob)
}

/// Decode a stored snapshot into its metadata and data.
fn unwrap(blob: &[u8]) -> std::io::Result<(SnapshotMeta, Vec<u8>)> {
    let malformed = || std::io::Error::other("malformed stored snapshot");
    let len = blob.get(..4).ok_or_else(malformed)?;
    let len = u32::from_be_bytes(len.try_into().map_err(|_| malformed())?) as usize;
    let header = blob.get(4..4 + len).ok_or_else(malformed)?;
    Ok((decode(header)?, blob[4 + len..].to_vec()))
}

/// The meta entries a snapshot carries with the data.
fn snapshot_meta_keys() -> Vec<Vec<u8>> {
    vec![APPLIED.to_vec(), MEMBERSHIP.to_vec()]
}

/// A replica's state machine.
#[derive(Debug)]
pub struct StateMachine {
    /// The replica's storage.
    replica: Replica,
    /// Where applied indexes are published.
    applied: watch::Sender<u64>,
    /// The last applied log ID, as storage holds it.
    last_applied: Option<LogId>,
    /// The last applied membership, as storage holds it.
    membership: StoredMembership,
    /// Whether an interrupted snapshot import has been checked for.
    recovered: bool,
}

impl StateMachine {
    /// Apply to `replica`, publishing applied indexes on `applied`.
    #[must_use]
    pub fn new(replica: Replica, applied: watch::Sender<u64>) -> Self {
        Self {
            replica,
            applied,
            last_applied: None,
            membership: StoredMembership::default(),
            recovered: false,
        }
    }

    /// Redo a snapshot import a crash interrupted, from the stored snapshot.
    async fn recover(&mut self) -> std::io::Result<()> {
        if self.recovered {
            return Ok(());
        }
        if self.replica.import_pending().await.map_err(io)? {
            let blob = self
                .replica
                .stored_snapshot()
                .await
                .map_err(io)?
                .ok_or_else(|| {
                    std::io::Error::other("an import is pending but no snapshot is stored")
                })?;
            let (_, data) = unwrap(&blob)?;
            self.replica.import(data).await.map_err(io)?;
        }
        self.recovered = true;
        Ok(())
    }

    /// Install `data` as this replica's state, as `meta` describes, after
    /// storing it durably.
    async fn install(&mut self, meta: &SnapshotMeta, data: Vec<u8>) -> std::io::Result<()> {
        self.replica
            .store_snapshot(wrap(meta, &data)?)
            .await
            .map_err(io)?;
        self.replica.import(data).await.map_err(io)?;
        self.last_applied = meta.last_log_id;
        self.membership = meta.last_membership.clone();
        self.applied
            .send_replace(meta.last_log_id.as_ref().map_or(0, openraft::LogId::index));
        Ok(())
    }
}

/// Builds a snapshot from a view captured between applies.
#[derive(Debug)]
pub struct SnapshotBuilder {
    /// The replica's storage, where the snapshot is stored.
    replica: Replica,
    /// The captured view, until the snapshot is built.
    view: Option<SnapshotView>,
    /// The snapshot's Raft metadata, as of the view.
    meta: SnapshotMeta,
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    type SnapshotData = SnapshotData;

    /// Export the captured view, store it with its metadata and return it.
    async fn build_snapshot(&mut self) -> Result<Snapshot, std::io::Error> {
        let view = self
            .view
            .take()
            .ok_or_else(|| std::io::Error::other("a snapshot builder builds once"))?;
        let data = view.export(snapshot_meta_keys()).await.map_err(io)?;
        self.replica
            .store_snapshot(wrap(&self.meta, &data)?)
            .await
            .map_err(io)?;
        Ok(Snapshot {
            meta: self.meta.clone(),
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotData = SnapshotData;
    type SnapshotBuilder = SnapshotBuilder;

    /// Return the last applied log ID and membership, after redoing any
    /// interrupted snapshot import.
    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), std::io::Error> {
        self.recover().await?;
        let applied: Option<LogId> = match self.replica.meta(APPLIED.to_vec()).await.map_err(io)? {
            Some(bytes) => decode(&bytes)?,
            None => None,
        };
        let membership = match self.replica.meta(MEMBERSHIP.to_vec()).await.map_err(io)? {
            Some(bytes) => decode(&bytes)?,
            None => StoredMembership::default(),
        };
        self.last_applied = applied;
        self.membership = membership.clone();
        self.applied
            .send_replace(applied.as_ref().map_or(0, openraft::LogId::index));
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
                    self.membership = stored;
                    None
                }
            };
            self.replica.apply(batch, meta).await.map_err(io)?;
            self.last_applied = Some(log_id);
            self.applied.send_replace(log_id.index());
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }

    /// Capture the state as it is now, between applies, for a snapshot.
    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        // A capture failure surfaces when the snapshot is built.
        let view = self.replica.capture().await.ok();
        SnapshotBuilder {
            replica: self.replica.clone(),
            view,
            meta: SnapshotMeta {
                last_log_id: self.last_applied,
                last_membership: self.membership.clone(),
            },
        }
    }

    /// Install a snapshot received from the leader.
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: SnapshotData,
    ) -> Result<(), std::io::Error> {
        self.install(meta, snapshot.into_inner()).await
    }

    /// Return the stored snapshot, if any.
    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot>, std::io::Error> {
        let Some(blob) = self.replica.stored_snapshot().await.map_err(io)? else {
            return Ok(None);
        };
        let (meta, data) = unwrap(&blob)?;
        Ok(Some(Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }
}
