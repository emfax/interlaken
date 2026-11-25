//! The OTLP exporter as a layer beside the program's own (ADR-0039).

#![cfg(feature = "otlp")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::Subscriber;
use tracing::span::{Attributes, Id};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// A layer standing in for a program's own log output: it counts the spans
/// it sees.
struct Counting(Arc<AtomicUsize>);

impl<S: Subscriber> Layer<S> for Counting {
    /// Count a new span.
    fn on_new_span(&self, _: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
/// Assert spans reach both the collector and a layer composed beside the
/// exporter.
fn layer_composes_with_other_layers() {
    let (port, received) = common::collector::start();
    let seen = Arc::new(AtomicUsize::new(0));
    let (otlp, guard) = interlaken_telemetry::otlp_layer(
        &format!("http://127.0.0.1:{port}/v1/traces"),
        "interlaken-test",
    )
    .unwrap();
    let subscriber = tracing_subscriber::registry()
        .with(otlp)
        .with(Counting(seen.clone()));
    tracing::subscriber::set_global_default(subscriber).unwrap();
    tracing::info_span!("interlaken.statement", kind = "select").in_scope(|| {});
    drop(guard);
    let request = received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collector received the spans");
    assert_eq!(request.line, "POST /v1/traces HTTP/1.1");
    assert_eq!(request.kind, "application/x-protobuf");
    assert!(request.size > 0);
    assert_eq!(seen.load(Ordering::Relaxed), 1);
}
