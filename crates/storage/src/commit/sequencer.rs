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

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::clock::CommitClock;
use super::history::CommitHistory;
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
}

impl Sequencer {
    /// Start the sequencer, continuing after `last`, the last durable commit timestamp.
    pub(crate) fn start(
        engine: Engine,
        clock: Arc<CommitClock>,
        last: Timestamp,
        config: SequencerConfig,
        stats: Arc<SequencerStats>,
    ) -> Result<Self> {
        let (sender, receiver) = flume::bounded(config.queue_capacity.max(1));
        let thread = thread::Builder::new()
            .name("interlaken-sequencer".to_owned())
            .spawn(move || {
                let mut state = State {
                    engine,
                    clock,
                    history: CommitHistory::new(last),
                    last,
                    config,
                    stats,
                };
                state.run(&receiver);
            })
            .map_err(|_| Error::PoolUnavailable("failed to spawn the commit sequencer"))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
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

impl Drop for Sequencer {
    /// Close the queue, let queued commits finish, and join the thread.
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
        {
            // A panic has already failed every waiting caller.
            let _ = thread.join();
        }
    }
}

/// State owned by the sequencer thread.
struct State {
    engine: Engine,
    clock: Arc<CommitClock>,
    history: CommitHistory,
    /// Last timestamp written to fjall.
    last: Timestamp,
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

        let validated = Instant::now();
        let written = self.engine.commit_group(
            next,
            accepted.iter().flat_map(|(ts, submission)| {
                submission
                    .request
                    .writes
                    .iter()
                    .map(|w| (*ts, &w.key[..], w.op.value().map(|v| &v[..])))
            }),
            self.config.durability,
        );
        let write_time = validated.elapsed();
        match written {
            Ok(()) => {
                let committed = accepted.len() as u64;
                self.last = next;
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
}
