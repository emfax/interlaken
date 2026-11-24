//! Exporting spans over OTLP.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;

use crate::InstallError;

/// Keeps OTLP export running; dropping it flushes the spans not yet sent
/// and stops the exporter.
#[must_use = "dropping the guard stops exporting spans"]
pub struct OtlpGuard {
    /// The tracer provider spans go through.
    provider: SdkTracerProvider,
}

impl std::fmt::Debug for OtlpGuard {
    /// Render the guard without its provider.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpGuard").finish_non_exhaustive()
    }
}

impl Drop for OtlpGuard {
    /// Flush pending spans and stop the exporter.
    fn drop(&mut self) {
        let _ = self.provider.shutdown();
    }
}

/// Install a tracing subscriber that exports every span to an OTLP
/// collector at `endpoint` (such as `http://localhost:4318/v1/traces`),
/// over HTTP with protobuf, as service `service`.
///
/// Spans are sent in batches from a thread of the exporter's own, so this
/// works with or without a Tokio runtime. Plain HTTP only: send to a
/// collector on the same host or network, which can forward over TLS.
/// Metrics aren't exported this way; serve them with
/// [`install_prometheus`](crate::install_prometheus), which an
/// OpenTelemetry collector can scrape.
///
/// # Errors
///
/// [`InstallError`] if the exporter can't be built or another tracing
/// subscriber is installed.
///
/// # Examples
///
/// ```no_run
/// let _guard = interlaken_telemetry::install_otlp("http://localhost:4318/v1/traces", "my-app")?;
/// # Ok::<(), interlaken_telemetry::InstallError>(())
/// ```
pub fn install_otlp(endpoint: &str, service: &str) -> Result<OtlpGuard, InstallError> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| InstallError(e.to_string()))?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service.to_owned())
                .build(),
        )
        .build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("interlaken"));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .map_err(|e| InstallError(e.to_string()))?;
    Ok(OtlpGuard { provider })
}
