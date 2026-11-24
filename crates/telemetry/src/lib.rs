//! Telemetry for programs that embed Interlaken (ADR-0039).
//!
//! Interlaken's libraries record spans with [`tracing`] and metrics with
//! [`metrics`], and never install anything: the program embedding them
//! decides where telemetry goes. This crate holds what such a program, or a
//! test, installs:
//!
//! - [`install_test_recorder`] and [`install_test_subscriber`]: an
//!   in-memory metrics recorder and span subscriber, for tests that check a
//!   metric moved or a span was recorded.
//!
//! No Interlaken library depends on this crate.

mod recorder;
mod subscriber;

pub use recorder::{TestRecorder, install_test_recorder};
pub use subscriber::{CapturedSpan, TestSubscriber, install_test_subscriber};
