//! Serving metrics for Prometheus to scrape.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

use crate::InstallError;

/// Bucket bounds for latencies, in seconds: 100 µs to 60 s.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0, 30.0, 60.0,
];

/// Bucket bounds for sizes, in bytes: 1 KiB to 1 GiB.
pub const SIZE_BUCKETS: &[f64] = &[
    1024.0,
    4096.0,
    16_384.0,
    65_536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
    67_108_864.0,
    268_435_456.0,
    1_073_741_824.0,
];

/// Bucket bounds for counts, such as transactions per group: 1 to 1,024.
pub const COUNT_BUCKETS: &[f64] = &[
    1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0,
];

/// Return a Prometheus builder with Interlaken's histogram buckets: latency
/// buckets for metrics ending `_seconds`, size buckets for `_bytes`, and
/// count buckets for `_size`. Other histograms are rendered as summaries.
///
/// # Errors
///
/// [`InstallError`] if a bucket list is refused (it never is: they aren't empty).
pub fn prometheus_builder() -> Result<PrometheusBuilder, InstallError> {
    let error = |e: metrics_exporter_prometheus::BuildError| InstallError(e.to_string());
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), LATENCY_BUCKETS)
        .map_err(error)?
        .set_buckets_for_metric(Matcher::Suffix("_bytes".into()), SIZE_BUCKETS)
        .map_err(error)?
        .set_buckets_for_metric(Matcher::Suffix("_size".into()), COUNT_BUCKETS)
        .map_err(error)
}

/// Install a Prometheus recorder as this process's metrics recorder,
/// serving `/metrics` over HTTP on `addr`, with Interlaken's histogram
/// buckets ([`prometheus_builder`]).
///
/// Called within a Tokio runtime, the server runs on it; otherwise on a
/// thread of its own. Call `interlaken_engine::describe_metrics()` after,
/// so the exporter shows each metric's description and unit.
///
/// # Errors
///
/// [`InstallError`] if `addr` can't be bound or another recorder is installed.
///
/// # Examples
///
/// ```no_run
/// interlaken_telemetry::install_prometheus("0.0.0.0:9464".parse()?)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn install_prometheus(addr: SocketAddr) -> Result<(), InstallError> {
    prometheus_builder()?
        .with_http_listener(addr)
        .install()
        .map_err(|e| InstallError(e.to_string()))
}
