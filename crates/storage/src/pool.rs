//! The storage pool: dedicated threads that run all fjall reads (ADR-0001).
//!
//! Async callers submit closures through a bounded queue and await the reply.
//! A full queue suspends the caller rather than blocking its thread, which is
//! the backpressure ADR-0001 asks for. If a caller stops waiting, its closure
//! still runs to completion and the result is discarded.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::{self, JoinHandle, ThreadId};

use crate::{Engine, Error, Result};

/// A unit of storage work.
type Job = Box<dyn FnOnce(&Engine) + Send>;

/// A fixed set of storage threads behind a bounded queue.
pub(crate) struct StoragePool {
    sender: Option<flume::Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl StoragePool {
    /// Start `threads` workers sharing `engine`, with a queue of `capacity` jobs.
    pub(crate) fn start(engine: &Engine, threads: usize, capacity: usize) -> Result<Self> {
        let (sender, receiver) = flume::bounded::<Job>(capacity.max(1));
        let workers = (0..threads.max(1))
            .map(|i| {
                let receiver = receiver.clone();
                let engine = engine.clone();
                thread::Builder::new()
                    .name(format!("interlaken-storage-{i}"))
                    .spawn(move || run_worker(&engine, &receiver))
            })
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|_| Error::PoolUnavailable("failed to spawn storage threads"))?;
        Ok(Self {
            sender: Some(sender),
            workers,
        })
    }

    /// Run `f` on a storage thread and return its result.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PoolUnavailable`] if the pool has shut down or `f` panicked.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Engine) -> T + Send + 'static,
    {
        let sender = self
            .sender
            .as_ref()
            .ok_or(Error::PoolUnavailable("shut down"))?;
        let (reply, response) = flume::bounded(1);
        let job: Job = Box::new(move |engine| {
            // The caller may have stopped waiting; its result is then discarded.
            let _ = reply.send(f(engine));
        });
        sender
            .send_async(job)
            .await
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        response
            .recv_async()
            .await
            .map_err(|_| Error::PoolUnavailable("storage task panicked"))
    }

    /// Return the IDs of the worker threads.
    fn worker_ids(&self) -> Vec<ThreadId> {
        self.workers.iter().map(|w| w.thread().id()).collect()
    }
}

impl Drop for StoragePool {
    /// Close the queue, let queued jobs finish, and join the workers.
    ///
    /// When the last reference is dropped on a worker thread itself, that
    /// worker can't be joined; the others are, and it exits once its current
    /// job returns.
    fn drop(&mut self) {
        let ids = self.worker_ids();
        self.sender = None;
        let current = thread::current().id();
        for (worker, id) in self.workers.drain(..).zip(ids) {
            if id != current {
                let _ = worker.join();
            }
        }
    }
}

/// Run jobs until the queue closes, containing panics so the worker survives.
fn run_worker(engine: &Engine, receiver: &flume::Receiver<Job>) {
    while let Ok(job) = receiver.recv() {
        // A panicking job drops its reply sender, so its caller sees an error.
        let _ = catch_unwind(AssertUnwindSafe(|| job(engine)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open an engine in a fresh temporary directory.
    fn engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        (dir, engine)
    }

    #[tokio::test]
    /// Assert jobs run on storage threads and return their results.
    async fn runs_jobs_on_storage_threads() {
        let (_dir, engine) = engine();
        let pool = StoragePool::start(&engine, 2, 4).unwrap();
        let name = pool
            .run(|_| thread::current().name().map(str::to_owned))
            .await
            .unwrap();
        assert!(name.unwrap().starts_with("interlaken-storage-"));
    }

    #[tokio::test]
    /// Assert a panicking job reports an error and the pool keeps working.
    async fn survives_panicking_job() {
        let (_dir, engine) = engine();
        let pool = StoragePool::start(&engine, 1, 4).unwrap();
        let err = pool.run(|_| -> u8 { panic!("boom") }).await.unwrap_err();
        assert!(matches!(
            err,
            Error::PoolUnavailable("storage task panicked")
        ));
        assert_eq!(pool.run(|_| 7).await.unwrap(), 7);
    }

    #[tokio::test]
    /// Assert many concurrent callers beyond the queue capacity all complete.
    async fn backpressure_does_not_lose_jobs() {
        let (_dir, engine) = engine();
        let pool = std::sync::Arc::new(StoragePool::start(&engine, 2, 1).unwrap());
        let tasks: Vec<_> = (0..64)
            .map(|i| {
                let pool = pool.clone();
                tokio::spawn(async move { pool.run(move |_| i).await.unwrap() })
            })
            .collect();
        let mut sum = 0;
        for task in tasks {
            sum += task.await.unwrap();
        }
        assert_eq!(sum, (0..64).sum::<i32>());
    }
}
