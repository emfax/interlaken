//! Telemetry for programs that embed Interlaken (ADR-0039).
//!
//! Interlaken's libraries record spans with [`tracing`] and metrics with
//! [`metrics`], and never install anything: the program embedding them
//! decides where telemetry goes. This crate holds what such a program, or a
//! test, installs:
//!
//! - [`install_prometheus`] (feature `prometheus`, on by default): serve
//!   metrics for Prometheus to scrape, with Interlaken's histogram buckets.
//! - [`install_otlp`] (feature `otlp`): export spans to an OpenTelemetry
//!   collector.
//! - [`install_test_recorder`] and [`install_test_subscriber`]: an
//!   in-memory metrics recorder and span subscriber, for tests that check a
//!   metric moved or a span was recorded.
//!
//! After installing a recorder, call `interlaken_engine::describe_metrics()`
//! so the exporter shows each metric's description and unit. Only one
//! metrics recorder and one tracing subscriber can be installed per process.
//!
//! No Interlaken library depends on this crate.

#[cfg(feature = "otlp")]
mod otlp;
#[cfg(feature = "prometheus")]
mod prometheus;
mod recorder;
mod subscriber;

#[cfg(feature = "otlp")]
pub use otlp::{OtlpGuard, install_otlp};
#[cfg(feature = "prometheus")]
pub use prometheus::{
    COUNT_BUCKETS, LATENCY_BUCKETS, SIZE_BUCKETS, install_prometheus, prometheus_builder,
};

pub use recorder::{TestRecorder, install_test_recorder};
pub use subscriber::{CapturedSpan, TestSubscriber, install_test_subscriber};

/// Why an exporter couldn't be installed.
#[derive(Debug, thiserror::Error)]
#[error("telemetry exporter not installed: {0}")]
pub struct InstallError(pub String);
