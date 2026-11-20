//! Cooperative scheduling (`executor-design.md`, Execution model).
//!
//! Operators that consume their whole input before producing anything (hash
//! builds, sorts, aggregations, duplicate elimination) can run for a long
//! time without awaiting anything that is pending, when their input comes
//! from memory rather than storage. A [`Yielder`] makes such a loop give the
//! thread back to the async runtime every [`YIELD_EVERY`] batches, so one
//! large query can't starve other connections. Yielding wakes the task
//! before returning `Pending`, which Tokio treats as a yield: the task goes
//! to the back of the run queue.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Input batches a blocking operator consumes between yields.
pub(crate) const YIELD_EVERY: u32 = 4;

/// Counts the batches a loop consumes and yields every [`YIELD_EVERY`].
#[derive(Debug, Default)]
pub(crate) struct Yielder {
    /// Batches consumed since the last yield.
    batches: u32,
}

impl Yielder {
    /// Record one consumed batch, yielding to the runtime if it is the [`YIELD_EVERY`]th.
    pub(crate) async fn tick(&mut self) {
        self.batches += 1;
        if self.batches >= YIELD_EVERY {
            self.batches = 0;
            YieldNow { yielded: false }.await;
        }
    }
}

/// A future that is pending exactly once, waking its task first.
struct YieldNow {
    /// Whether it has returned `Pending` already.
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    /// Return `Pending` (after scheduling a wake-up) on the first poll and `Ready` after.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert a yielder is pending once on every `YIELD_EVERY`th tick and never otherwise.
    fn yields_every_few_batches() {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut yielder = Yielder::default();
        let mut pending = 0;
        for _ in 0..(3 * YIELD_EVERY) {
            let mut tick = Box::pin(yielder.tick());
            loop {
                match tick.as_mut().poll(&mut cx) {
                    Poll::Ready(()) => break,
                    Poll::Pending => pending += 1,
                }
            }
        }
        assert_eq!(pending, 3);
    }
}
