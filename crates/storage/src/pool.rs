//! The storage pool: dedicated threads that run all fjall reads (ADR-0001).
//!
//! Async callers submit closures through a bounded queue and await the reply.
//! A full queue suspends the caller rather than blocking its thread, which is
//! the backpressure ADR-0001 asks for. If a caller stops waiting, its closure
//! still runs to completion and the result is discarded.
//!
//! Background work (version GC) goes through a second, low-priority queue
//! (ADR-0001, ADR-0008). Workers block only on the normal queue, which wakes
//! them reliably on shutdown; submitting a low-priority job also sends a
//! wake-up token through the normal queue. The worker that takes the token
//! first runs the normal jobs that were already waiting, then the
//! low-priority job.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::{self, JoinHandle, ThreadId};

use crate::{Engine, Error, Result};

/// A unit of storage work.
type Job = Box<dyn FnOnce(&Engine) + Send>;

/// A message on the normal queue.
enum Message {
    /// Run this job.
    Run(Job),
    /// A low-priority job is waiting in the low queue.
    Low,
}

/// A fixed set of storage threads behind a bounded queue and a low-priority queue.
pub(crate) struct StoragePool {
    sender: Option<flume::Sender<Message>>,
    low: Option<flume::Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl StoragePool {
    /// Start `threads` workers sharing `engine`, with a queue of `capacity` jobs.
    pub(crate) fn start(engine: &Engine, threads: usize, capacity: usize) -> Result<Self> {
        let (sender, receiver) = flume::bounded::<Message>(capacity.max(1));
        let (low, low_receiver) = flume::unbounded::<Job>();
        let workers = (0..threads.max(1))
            .map(|i| {
                let receiver = receiver.clone();
                let low_receiver = low_receiver.clone();
                let engine = engine.clone();
                thread::Builder::new()
                    .name(format!("interlaken-storage-{i}"))
                    .spawn(move || run_worker(&engine, &receiver, &low_receiver))
            })
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|_| Error::PoolUnavailable("failed to spawn storage threads"))?;
        Ok(Self {
            sender: Some(sender),
            low: Some(low),
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
        let (job, response) = Self::job(f);
        sender
            .send_async(Message::Run(job))
            .await
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        Self::await_reply(response).await
    }

    /// Run `f` on a storage thread at low priority and return its result.
    ///
    /// # Errors
    ///
    /// As for [`run`](Self::run).
    pub(crate) async fn run_low<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Engine) -> T + Send + 'static,
    {
        let (sender, low) = self.senders()?;
        let (job, response) = Self::job(f);
        low.send(job)
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        sender
            .send_async(Message::Low)
            .await
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        Self::await_reply(response).await
    }

    /// Run `f` at low priority, blocking the calling (non-async) thread until it returns.
    ///
    /// # Errors
    ///
    /// As for [`run`](Self::run).
    pub(crate) fn run_low_blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Engine) -> T + Send + 'static,
    {
        let (sender, low) = self.senders()?;
        let (job, response) = Self::job(f);
        low.send(job)
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        sender
            .send(Message::Low)
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        response
            .recv()
            .map_err(|_| Error::PoolUnavailable("storage task panicked"))
    }

    /// Return both queue senders, or an error after shutdown.
    fn senders(&self) -> Result<(&flume::Sender<Message>, &flume::Sender<Job>)> {
        match (&self.sender, &self.low) {
            (Some(sender), Some(low)) => Ok((sender, low)),
            _ => Err(Error::PoolUnavailable("shut down")),
        }
    }

    /// Await a job's result.
    async fn await_reply<T>(response: flume::Receiver<T>) -> Result<T> {
        response
            .recv_async()
            .await
            .map_err(|_| Error::PoolUnavailable("storage task panicked"))
    }

    /// Wrap `f` as a job that sends its result to the returned receiver.
    fn job<T, F>(f: F) -> (Job, flume::Receiver<T>)
    where
        T: Send + 'static,
        F: FnOnce(&Engine) -> T + Send + 'static,
    {
        let (reply, response) = flume::bounded(1);
        let job: Job = Box::new(move |engine| {
            // The caller may have stopped waiting; its result is then discarded.
            let _ = reply.send(f(engine));
        });
        (job, response)
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
        self.low = None;
        let current = thread::current().id();
        for (worker, id) in self.workers.drain(..).zip(ids) {
            if id != current {
                let _ = worker.join();
            }
        }
    }
}

/// Run jobs until the normal queue closes.
fn run_worker(engine: &Engine, normal: &flume::Receiver<Message>, low: &flume::Receiver<Job>) {
    while let Ok(message) = normal.recv() {
        match message {
            Message::Run(job) => run_job(engine, job),
            Message::Low => {
                // Normal jobs already waiting go first. The drain is bounded
                // by the current queue length so a busy pool can't starve GC.
                let mut low_jobs = 1;
                for _ in 0..normal.len() {
                    match normal.try_recv() {
                        Ok(Message::Run(job)) => run_job(engine, job),
                        Ok(Message::Low) => low_jobs += 1,
                        Err(_) => break,
                    }
                }
                for _ in 0..low_jobs {
                    if let Ok(job) = low.try_recv() {
                        run_job(engine, job);
                    }
                }
            }
        }
    }
}

/// Run one job, containing a panic so the worker survives.
fn run_job(engine: &Engine, job: Job) {
    // A panicking job drops its reply sender, so its caller sees an error.
    let _ = catch_unwind(AssertUnwindSafe(|| job(engine)));
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
    /// Assert low-priority jobs run, including from a blocking thread, and shutdown completes.
    async fn low_priority_jobs_run() {
        let (_dir, engine) = engine();
        let pool = std::sync::Arc::new(StoragePool::start(&engine, 1, 2).unwrap());
        assert_eq!(pool.run_low(|_| 3).await.unwrap(), 3);
        let blocking = pool.clone();
        let value = tokio::task::spawn_blocking(move || blocking.run_low_blocking(|_| 4))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value, 4);
    }

    #[test]
    /// Assert a worker prefers normal jobs over waiting low-priority ones.
    fn normal_jobs_go_first() {
        let (normal_tx, normal) = flume::unbounded::<Message>();
        let (low_tx, low) = flume::unbounded::<Job>();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = |tag: &'static str| -> Job {
            let order = order.clone();
            Box::new(move |_| order.lock().unwrap().push(tag))
        };
        // The low job is submitted first, but the normal job was already waiting.
        low_tx.send(record("low")).unwrap();
        normal_tx.send(Message::Low).unwrap();
        normal_tx.send(Message::Run(record("normal"))).unwrap();
        drop((normal_tx, low_tx));
        let (_dir, engine) = engine();
        run_worker(&engine, &normal, &low);
        assert_eq!(*order.lock().unwrap(), ["normal", "low"]);
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
