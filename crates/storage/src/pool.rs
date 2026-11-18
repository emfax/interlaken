//! The storage pool: dedicated threads that run all fjall reads (ADR-0001).
//!
//! Async callers submit closures through a bounded queue and await the reply.
//! A full queue suspends the caller rather than blocking its thread, which is
//! the backpressure ADR-0001 asks for. If a caller stops waiting, its closure
//! still runs to completion and the result is discarded.
//!
//! Background work (version GC) is submitted as low priority (ADR-0001,
//! ADR-0008). It travels through the same queue, so workers block on one
//! receiver that always wakes on shutdown, and a cancelled submission
//! leaves nothing behind. A worker that takes a low-priority job first runs
//! the normal jobs already waiting behind it, then the low-priority job.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::{self, JoinHandle, ThreadId};

use crate::{Engine, Error, Result};

/// A unit of storage work.
type Job = Box<dyn FnOnce(&Engine) + Send>;

/// A message on the normal queue.
enum Message {
    /// Run this job.
    Run(Job),
    /// Run this job after the normal jobs already queued.
    Low(Job),
}

/// A fixed set of storage threads behind a bounded queue and a low-priority queue.
pub(crate) struct StoragePool {
    sender: Option<flume::Sender<Message>>,
    workers: Vec<JoinHandle<()>>,
}

impl StoragePool {
    /// Start `threads` workers sharing `engine`, with a queue of `capacity` jobs.
    pub(crate) fn start(engine: &Engine, threads: usize, capacity: usize) -> Result<Self> {
        let (sender, receiver) = flume::bounded::<Message>(capacity.max(1));
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
        let (job, response) = Self::job(f);
        self.sender()?
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
        let (job, response) = Self::job(f);
        self.sender()?
            .send_async(Message::Low(job))
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
        let (job, response) = Self::job(f);
        self.sender()?
            .send(Message::Low(job))
            .map_err(|_| Error::PoolUnavailable("shut down"))?;
        response
            .recv()
            .map_err(|_| Error::PoolUnavailable("storage task panicked"))
    }

    /// Return the queue sender, or an error after shutdown.
    fn sender(&self) -> Result<&flume::Sender<Message>> {
        self.sender
            .as_ref()
            .ok_or(Error::PoolUnavailable("shut down"))
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
        let current = thread::current().id();
        for (worker, id) in self.workers.drain(..).zip(ids) {
            if id != current {
                let _ = worker.join();
            }
        }
    }
}

/// Run jobs until the queue closes.
fn run_worker(engine: &Engine, queue: &flume::Receiver<Message>) {
    while let Ok(message) = queue.recv() {
        match message {
            Message::Run(job) => run_job(engine, job),
            Message::Low(first) => {
                // Normal jobs already waiting go first. The drain is bounded
                // by the current queue length so a busy pool can't starve GC.
                let mut low = vec![first];
                for _ in 0..queue.len() {
                    match queue.try_recv() {
                        Ok(Message::Run(job)) => run_job(engine, job),
                        Ok(Message::Low(job)) => low.push(job),
                        Err(_) => break,
                    }
                }
                for job in low {
                    run_job(engine, job);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    /// Assert cancelled low-priority submissions leave no stranded work behind.
    async fn cancelled_low_priority_submissions_are_harmless() {
        let (_dir, engine) = engine();
        let pool = std::sync::Arc::new(StoragePool::start(&engine, 1, 1).unwrap());
        // Block the only worker, then fill the one-slot queue.
        let (release, blocked) = flume::bounded::<()>(0);
        let blocker = pool.clone();
        let busy = tokio::spawn(async move { blocker.run(move |_| blocked.recv().ok()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let filler = pool.clone();
        let queued = tokio::spawn(async move { filler.run(|_| 1).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // These submissions can't get into the full queue and are cancelled.
        for _ in 0..3 {
            let attempt = pool.run_low(|_| 2);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(5), attempt)
                    .await
                    .is_err()
            );
        }
        release.send(()).unwrap();
        busy.await.unwrap().unwrap();
        assert_eq!(queued.await.unwrap().unwrap(), 1);
        // Later low-priority work still completes, and the pool shuts down.
        let blocking = pool.clone();
        let value = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || blocking.run_low_blocking(|_| 3)),
        )
        .await
        .expect("low-priority job stranded")
        .unwrap()
        .unwrap();
        assert_eq!(value, 3);
        drop(pool);
    }

    #[test]
    /// Assert a worker prefers normal jobs over waiting low-priority ones.
    fn normal_jobs_go_first() {
        let (tx, queue) = flume::unbounded::<Message>();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = |tag: &'static str| -> Job {
            let order = order.clone();
            Box::new(move |_| order.lock().unwrap().push(tag))
        };
        // The low job is submitted first, but the normal job was already waiting.
        tx.send(Message::Low(record("low"))).unwrap();
        tx.send(Message::Run(record("normal"))).unwrap();
        drop(tx);
        let (_dir, engine) = engine();
        run_worker(&engine, &queue);
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
