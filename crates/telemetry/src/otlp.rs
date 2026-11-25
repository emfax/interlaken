//! Exporting spans over OTLP.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

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

/// Build a tracing layer that exports every span to an OTLP collector at
/// `endpoint` (such as `http://localhost:4318/v1/traces`), over HTTP with
/// protobuf, as service `service`.
///
/// Compose the layer with the program's own layers (log output, filters)
/// and install the result as the global subscriber; keep the guard alive
/// for as long as spans should be exported. [`install_otlp`] does this for
/// programs that want OTLP export and nothing else.
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
/// [`InstallError`] if the exporter can't be built.
///
/// # Examples
///
/// ```no_run
/// use tracing_subscriber::layer::SubscriberExt;
///
/// let (otlp, _guard) =
///     interlaken_telemetry::otlp_layer("http://localhost:4318/v1/traces", "my-app")?;
/// let subscriber = tracing_subscriber::registry().with(otlp);
/// tracing::subscriber::set_global_default(subscriber).expect("no other subscriber");
/// # Ok::<(), interlaken_telemetry::InstallError>(())
/// ```
pub fn otlp_layer<S>(
    endpoint: &str,
    service: &str,
) -> Result<(impl Layer<S>, OtlpGuard), InstallError>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
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
    Ok((layer, OtlpGuard { provider }))
}

/// Install a tracing subscriber that exports every span to an OTLP
/// collector at `endpoint`, and nothing else; see [`otlp_layer`] for the
/// transport, and to combine the export with the program's own layers.
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
    let (layer, guard) = otlp_layer(endpoint, service)?;
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .map_err(|e| InstallError(e.to_string()))?;
    Ok(guard)
}
