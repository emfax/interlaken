//! Exporting spans over OTLP (ADR-0039 phase 5).

#![cfg(feature = "otlp")]

mod common;

#[test]
/// Assert spans reach a collector as OTLP over HTTP with protobuf, at the
/// latest when the guard is dropped.
fn spans_are_exported() {
    let (port, received) = common::collector::start();
    let guard = interlaken_telemetry::install_otlp(
        &format!("http://127.0.0.1:{port}/v1/traces"),
        "interlaken-test",
    )
    .unwrap();
    tracing::info_span!("interlaken.statement", kind = "select").in_scope(|| {});
    drop(guard);
    let request = received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collector received the spans");
    assert_eq!(request.line, "POST /v1/traces HTTP/1.1");
    assert_eq!(request.kind, "application/x-protobuf");
    assert!(request.size > 0);
}
