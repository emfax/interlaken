//! The Raft log and vote, kept in the replica's storage (ADR-0037).
//!
//! Entries live in storage's `replica_log` keyspace by index, postcard
//! encoded. The vote, the last purged log ID and the committed log ID live
//! in `meta`. Raft is correct only if the vote and appended entries are
//! durable before this node answers, so under `Durability::Sync` both are
//! synced before they are acknowledged.

use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};

use interlaken_storage::replica::Replica;
use openraft::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};

use crate::types::{Entry, LogId, TypeConfig, Vote, decode, encode, io};

/// The `meta` key of the vote.
const VOTE: &[u8] = b"raft_vote";
/// The `meta` key of the last purged log ID.
const PURGED: &[u8] = b"raft_purged";
/// The `meta` key of the committed log ID.
const COMMITTED: &[u8] = b"raft_committed";

/// A replica's Raft log storage. Cloning is cheap; clones share storage.
#[derive(Clone, Debug)]
pub struct LogStore {
    /// The replica's storage.
    replica: Replica,
    /// Whether appends and votes are synced before they are acknowledged.
    sync: bool,
}

impl LogStore {
    /// Keep the log in `replica`'s storage, syncing votes and appends if `sync`.
    #[must_use]
    pub fn new(replica: Replica, sync: bool) -> Self {
        Self { replica, sync }
    }

    /// Read and decode the `meta` entry at `key`, if present.
    async fn read<T: for<'de> serde::Deserialize<'de>>(
        &self,
        key: &[u8],
    ) -> std::io::Result<Option<T>> {
        self.replica
            .meta(key.to_vec())
            .await
            .map_err(io)?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    /// Encode and write the `meta` entry at `key`, synced if `sync`.
    async fn write<T: serde::Serialize>(
        &self,
        key: &[u8],
        value: &T,
        sync: bool,
    ) -> std::io::Result<()> {
        self.replica
            .set_meta(vec![(key.to_vec(), encode(value)?)], sync)
            .await
            .map_err(io)
    }
}

/// Return the half-open index range `start..end` that `range` covers.
fn bounds(range: &impl RangeBounds<u64>) -> (u64, u64) {
    let start = match range.start_bound() {
        Bound::Included(&s) => s,
        Bound::Excluded(&s) => s.saturating_add(1),
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&e) => e.saturating_add(1),
        Bound::Excluded(&e) => e,
        Bound::Unbounded => u64::MAX,
    };
    (start, end)
}

impl RaftLogReader<TypeConfig> for LogStore {
    /// Return the entries whose indexes `range` covers, in order.
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, std::io::Error> {
        let (start, end) = bounds(&range);
        self.replica
            .log_range(start, end)
            .await
            .map_err(io)?
            .iter()
            .map(|(_, bytes)| decode(bytes))
            .collect()
    }

    /// Return the saved vote, if any.
    async fn read_vote(&mut self) -> Result<Option<Vote>, std::io::Error> {
        self.read(VOTE).await
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    /// Return the last purged log ID and the last log ID.
    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, std::io::Error> {
        let purged: Option<LogId> = self.read(PURGED).await?;
        let last = match self.replica.log_bounds().await.map_err(io)? {
            Some((_, (_, bytes))) => Some(decode::<Entry>(&bytes)?.log_id),
            None => purged,
        };
        Ok(LogState {
            last_purged_log_id: purged,
            last_log_id: last,
        })
    }

    /// Return a reader sharing this storage.
    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    /// Save the vote, durably under `Durability::Sync`, before returning.
    async fn save_vote(&mut self, vote: &Vote) -> Result<(), std::io::Error> {
        self.write(VOTE, vote, self.sync).await
    }

    /// Save the committed log ID, so a restarted node can apply up to it.
    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), std::io::Error> {
        self.write(COMMITTED, &committed, false).await
    }

    /// Return the saved committed log ID.
    async fn read_committed(&mut self) -> Result<Option<LogId>, std::io::Error> {
        Ok(self.read::<Option<LogId>>(COMMITTED).await?.flatten())
    }

    /// Append entries in one batch, durably under `Durability::Sync`, and
    /// report completion through `callback`.
    async fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<TypeConfig>,
    ) -> Result<(), std::io::Error>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let encoded = entries
            .into_iter()
            .map(|entry| Ok((entry.log_id.index(), encode(&entry)?)))
            .collect::<std::io::Result<Vec<_>>>()?;
        let result = self
            .replica
            .log_append(encoded, self.sync)
            .await
            .map_err(io);
        let reported = result
            .as_ref()
            .copied()
            .map_err(|e| std::io::Error::other(e.to_string()));
        callback.io_completed(reported);
        result
    }

    /// Remove every entry after `last_log_id`, or all if it is `None`.
    async fn truncate_after(&mut self, last_log_id: Option<LogId>) -> Result<(), std::io::Error> {
        let start = last_log_id.map_or(0, |id| id.index() + 1);
        self.replica
            .log_remove(start, u64::MAX, self.sync)
            .await
            .map_err(io)
    }

    /// Remove every entry up to and including `log_id`, and remember it as purged.
    async fn purge(&mut self, log_id: LogId) -> Result<(), std::io::Error> {
        self.write(PURGED, &log_id, self.sync).await?;
        self.replica
            .log_remove(0, log_id.index() + 1, false)
            .await
            .map_err(io)
    }
}
