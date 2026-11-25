//! The commit sequencer thread: validation, group commit and durability (ADR-0005).
//!
//! One thread receives commit requests through a bounded queue. Each
//! iteration takes every request already waiting (up to the group limit),
//! validates them in arrival order against recent commits and against the
//! earlier members of the group, assigns consecutive timestamps to the
//! survivors, and writes the whole group as one fjall batch. Only once the
//! batch is in fjall (and synced, under [`Durability::Sync`]) does
//! `visible_ts` advance and the callers hear back.
//!
//! If writing a group fails, the outcome on disk is unknown: fjall may have
//! journaled the batch. The sequencer then answers the group with
//! [`Error::CommitOutcomeUnknown`] and stops, so no later commit can reuse
//! those timestamps; reopening the database recovers.
//!
//! **The commit log** (ADR-0037 phase 1, `StorageOptions::commit_log`).
//! Commits can instead go through a log, as replication needs. The
//! sequencer thread then *proposes*: it validates exactly as above, and
//! appends each group as one [`LogEntry`] to the `raftlog` keyspace,
//! synced as the durability setting says. That append is the commit. An
//! *apply* thread then writes each entry's versions to `data`, unsynced,
//! publishes `visible_ts` and answers the callers. The proposer validates
//! the next group while earlier ones are applied, against a history that
//! already holds them, since they come first in the log. Every few thousand
//! entries a compaction thread syncs and removes the applied entries.

use crate::changes::{ChangeRetention, entries_from};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::clock::CommitClock;
use super::history::CommitHistory;
use super::log::{LogEntry, LogWrite};
use super::request::CommitRequest;
use super::stats::SequencerStats;
use crate::{Durability, Engine, Error, Result, Timestamp};

/// Sequencer settings taken from [`StorageOptions`](crate::StorageOptions).
#[derive(Clone, Copy, Debug)]
pub(crate) struct SequencerConfig {
    /// When a commit is acknowledged.
    pub(crate) durability: Durability,
    /// Most transactions written in one group.
    pub(crate) max_group: usize,
    /// Bounded request queue length.
    pub(crate) queue_capacity: usize,
    /// Commit through the log, with separate proposal and apply (ADR-0037).
    pub(crate) log: bool,
}

/// How many applied log entries accumulate before the compaction thread
/// syncs `data` and removes them from the log.
const COMPACT_EVERY: u64 = 4096;

/// A logged group on its way to the apply thread, with its callers.
struct Logged {
    /// The entry to apply.
    entry: LogEntry,
    /// Each transaction's timestamp and where its outcome goes.
    replies: Vec<(Timestamp, flume::Sender<Result<Timestamp>>)>,
}

/// A queued request and where its outcome goes.
#[derive(Debug)]
struct Submission {
    request: CommitRequest,
    reply: flume::Sender<Result<Timestamp>>,
}

/// Handle to the sequencer thread. Dropping it drains the queue and joins the thread.
#[derive(Debug)]
pub(crate) struct Sequencer {
    sender: Option<flume::Sender<Submission>>,
    thread: Option<JoinHandle<()>>,
    /// The apply and compaction threads, under the commit log.
    applier: Option<JoinHandle<()>>,
    compactor: Option<JoinHandle<()>>,
}

impl Sequencer {
    /// Start the sequencer, continuing after `last`, the last durable commit
    /// timestamp, and `last_index`, the last commit log index.
    pub(crate) fn start(
        engine: Engine,
        clock: Arc<CommitClock>,
        last: Timestamp,
        last_index: u64,
        config: SequencerConfig,
        stats: Arc<SequencerStats>,
    ) -> Result<Self> {
        let (sender, receiver) = flume::bounded(config.queue_capacity.max(1));
        let (to_apply, applier, compactor) = if config.log {
            let (to_apply, logged) = flume::bounded::<Logged>(config.queue_capacity.max(1));
            // Compaction syncs, so it runs on its own thread and never
            // delays applying or answering commits.
            let (to_compact, compactable) = flume::bounded::<u64>(1);
            let compacting = engine.clone();
            let compactor = thread::Builder::new()
                .name("interlaken-log-compactor".to_owned())
                .spawn(move || compact(&compacting, &compactable, last_index))
                .map_err(|_| Error::PoolUnavailable("failed to spawn the log compactor"))?;
            let (engine, clock) = (engine.clone(), Arc::clone(&clock));
            let thread = thread::Builder::new()
                .name("interlaken-applier".to_owned())
                .spawn(move || apply(&engine, &clock, &logged, &to_compact))
                .map_err(|_| Error::PoolUnavailable("failed to spawn the commit applier"))?;
            (Some(to_apply), Some(thread), Some(compactor))
        } else {
            (None, None, None)
        };
        let thread = thread::Builder::new()
            .name("interlaken-sequencer".to_owned())
            .spawn(move || {
                let mut state = State {
                    engine,
                    clock,
                    history: CommitHistory::new(last),
                    last,
                    last_index,
                    to_apply,
                    config,
                    stats,
                };
                state.run(&receiver);
            })
            .map_err(|_| Error::PoolUnavailable("failed to spawn the commit sequencer"))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            applier,
            compactor,
        })
    }

    /// Submit a transaction and wait for its commit timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SerializationFailure`] if validation fails,
    /// [`Error::CommitOutcomeUnknown`] if writing its group failed, or
    /// [`Error::SequencerStopped`] if the sequencer is no longer running.
    pub(crate) async fn submit(&self, request: CommitRequest) -> Result<Timestamp> {
        let (reply, response) = flume::bounded(1);
        self.sender
            .as_ref()
            .ok_or(Error::SequencerStopped)?
            .send_async(Submission { request, reply })
            .await
            .map_err(|_| Error::SequencerStopped)?;
        response
            .recv_async()
            .await
            .map_err(|_| Error::SequencerStopped)?
    }
}

impl crate::replica::CommitPath for Sequencer {
    /// Submit through the sequencer thread.
    fn submit(&self, request: CommitRequest) -> crate::replica::CommitFuture<'_> {
        Box::pin(Sequencer::submit(self, request))
    }
}

impl Drop for Sequencer {
    /// Close the queue, let queued commits finish, and join the threads.
    fn drop(&mut self) {
        drop(self.sender.take());
        // The proposer's end of the apply queue drops with it, so the apply
        // thread finishes what was logged and stops after it.
        let threads = [
            self.thread.take(),
            self.applier.take(),
            self.compactor.take(),
        ];
        for thread in threads.into_iter().flatten() {
            if thread.thread().id() != thread::current().id() {
                // A panic has already failed every waiting caller.
                let _ = thread.join();
            }
        }
    }
}

/// State owned by the sequencer thread.
struct State {
    engine: Engine,
    clock: Arc<CommitClock>,
    history: CommitHistory,
    /// Last timestamp written to fjall, or under the commit log, proposed.
    last: Timestamp,
    /// Last commit log index proposed.
    last_index: u64,
    /// Where logged groups go to be applied, under the commit log.
    to_apply: Option<flume::Sender<Logged>>,
    config: SequencerConfig,
    stats: Arc<SequencerStats>,
}

impl State {
    /// Commit groups until the queue closes or a write fails.
    fn run(&mut self, receiver: &flume::Receiver<Submission>) {
        while let Ok(first) = receiver.recv() {
            let mut group = vec![first];
            while group.len() < self.config.max_group.max(1) {
                match receiver.try_recv() {
                    Ok(request) => group.push(request),
                    Err(_) => break,
                }
            }
            if self.commit_group(group).is_err() {
                return;
            }
        }
    }

    /// Validate, write and acknowledge one group.
    ///
    /// Rejected transactions hear back immediately. Returns `Err` if the
    /// group's write failed and the sequencer must stop.
    fn commit_group(&mut self, group: Vec<Submission>) -> Result<(), ()> {
        let span = tracing::info_span!(
            "interlaken.storage.commit_group",
            transactions = group.len(),
            bytes = tracing::field::Empty,
        );
        if !span.is_disabled() {
            let bytes: usize = group
                .iter()
                .flat_map(|submission| &submission.request.writes)
                .map(|w| w.key.len() + w.op.value().map_or(0, bytes::Bytes::len))
                .sum();
            span.record("bytes", bytes);
        }
        let _entered = span.enter();
        let started = Instant::now();
        let mut conflicts = 0;
        let mut accepted: Vec<(Timestamp, Submission)> = Vec::with_capacity(group.len());
        let mut next = self.last;
        for submission in group {
            if let Err(conflict) = self.history.validate(&submission.request) {
                let _ = submission
                    .reply
                    .send(Err(Error::SerializationFailure(conflict)));
                conflicts += 1;
                continue;
            }
            let Some(ts) = next.successor() else {
                let _ = submission.reply.send(Err(Error::TimestampExhausted));
                continue;
            };
            next = ts;
            // Later members of the group validate against this one.
            let keys = submission.request.writes.iter().map(|w| w.key.clone());
            self.history.record(ts, keys.collect());
            accepted.push((ts, submission));
        }
        if accepted.is_empty() {
            self.stats
                .record_round(0, conflicts, started.elapsed(), Duration::ZERO);
            return Ok(());
        }

        if self.to_apply.is_some() {
            return self.propose(accepted, next, started, conflicts);
        }
        let validated = Instant::now();
        let changes = self.clock.changes();
        let entries = if changes.retention() == ChangeRetention::Off {
            Vec::new()
        } else {
            entries_from(accepted.iter().flat_map(|(ts, submission)| {
                submission
                    .request
                    .writes
                    .iter()
                    .map(|w| (*ts, &w.key, w.op.value(), w.prior))
            }))
        };
        let written = self.engine.commit_group(
            next,
            accepted.iter().flat_map(|(ts, submission)| {
                submission
                    .request
                    .writes
                    .iter()
                    .map(|w| (*ts, &w.key[..], w.op.value().map(|v| &v[..])))
            }),
            if changes.durable() { &entries } else { &[] },
            self.config.durability,
        );
        let write_time = validated.elapsed();
        match written {
            Ok(()) => {
                let committed = accepted.len() as u64;
                self.last = next;
                self.clock.changes().record(&entries);
                self.clock.publish(next);
                for (ts, submission) in accepted {
                    let _ = submission.reply.send(Ok(ts));
                }
                self.history.trim(self.clock.horizon());
                let validation = started.elapsed().saturating_sub(write_time);
                self.stats
                    .record_round(committed, conflicts, validation, write_time);
                Ok(())
            }
            Err(error) => {
                let error = Arc::new(error);
                for (_, submission) in accepted {
                    let _ = submission
                        .reply
                        .send(Err(Error::CommitOutcomeUnknown(Arc::clone(&error))));
                }
                Err(())
            }
        }
    }

    /// Append a validated group, the last of whose timestamps is `next`, to
    /// the commit log, and hand it to the apply thread.
    ///
    /// Returns `Err` if the append failed or the apply thread has stopped,
    /// after answering the group: the sequencer must then stop.
    fn propose(
        &mut self,
        accepted: Vec<(Timestamp, Submission)>,
        next: Timestamp,
        started: Instant,
        conflicts: u64,
    ) -> Result<(), ()> {
        let entry = LogEntry {
            index: self.last_index + 1,
            first_ts: accepted[0].0,
            txns: accepted
                .iter()
                .map(|(_, submission)| {
                    submission
                        .request
                        .writes
                        .iter()
                        .map(|w| LogWrite {
                            key: w.key.clone(),
                            value: w.op.value().cloned(),
                            prior: w.prior,
                        })
                        .collect()
                })
                .collect(),
        };
        let appending = Instant::now();
        let appended = self.engine.append_log(&entry, self.config.durability);
        let append_time = appending.elapsed();
        let replies: Vec<(Timestamp, flume::Sender<Result<Timestamp>>)> = accepted
            .into_iter()
            .map(|(ts, submission)| (ts, submission.reply))
            .collect();
        let unknown = |replies: Vec<(Timestamp, flume::Sender<Result<Timestamp>>)>, error| {
            let error = Arc::new(error);
            for (_, reply) in replies {
                let _ = reply.send(Err(Error::CommitOutcomeUnknown(Arc::clone(&error))));
            }
        };
        if let Err(error) = appended {
            unknown(replies, error);
            return Err(());
        }
        self.last = next;
        self.last_index = entry.index;
        let committed = replies.len() as u64;
        let to_apply = self
            .to_apply
            .as_ref()
            .expect("proposing under the commit log");
        if let Err(flume::SendError(logged)) = to_apply.send(Logged { entry, replies }) {
            unknown(logged.replies, Error::SequencerStopped);
            return Err(());
        }
        self.history.trim(self.clock.horizon());
        let validation = started.elapsed().saturating_sub(append_time);
        self.stats
            .record_round(committed, conflicts, validation, append_time);
        Ok(())
    }
}

/// Apply logged groups in order until the proposer stops or a write fails:
/// write each entry to `data`, publish its timestamps, answer its callers,
/// and tell the compaction thread how far the log is applied.
fn apply(
    engine: &Engine,
    clock: &CommitClock,
    logged: &flume::Receiver<Logged>,
    to_compact: &flume::Sender<u64>,
) {
    while let Ok(Logged { entry, replies }) = logged.recv() {
        let changes = clock.changes();
        let entries = if changes.retention() == ChangeRetention::Off {
            Vec::new()
        } else {
            entry.changes()
        };
        let durable = if changes.durable() { &entries[..] } else { &[] };
        match engine.apply_entry(&entry, durable) {
            Ok(()) => {
                changes.record(&entries);
                clock.publish(entry.last_ts());
                for (ts, reply) in replies {
                    let _ = reply.send(Ok(ts));
                }
            }
            Err(error) => {
                // The entry is committed in the log, and opening the
                // database applies it, but this process can't say so.
                let error = Arc::new(error);
                for (_, reply) in replies {
                    let _ = reply.send(Err(Error::CommitOutcomeUnknown(Arc::clone(&error))));
                }
                return;
            }
        }
        // A full queue means the compactor is busy; it hears a later index.
        let _ = to_compact.try_send(entry.index);
    }
}

/// Remove applied entries from the log once [`COMPACT_EVERY`] have
/// accumulated past `compacted`, until the apply thread stops.
fn compact(engine: &Engine, applied: &flume::Receiver<u64>, mut compacted: u64) {
    while let Ok(index) = applied.recv() {
        // Compaction only saves space: a failure leaves the entries, which
        // opening the database skips since they are applied.
        if index >= compacted + COMPACT_EVERY && engine.compact_log(index).is_ok() {
            compacted = index;
        }
    }
}
