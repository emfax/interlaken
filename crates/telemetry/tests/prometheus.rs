//! Serving metrics for Prometheus (ADR-0039 phase 5).

#![cfg(feature = "prometheus")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Fetch `/metrics` from `port`, retrying while the server starts.
fn scrape(port: u16) -> String {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            return body;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the metrics server didn't start");
}

#[test]
/// Assert metrics are served with their descriptions and Interlaken's buckets.
fn metrics_are_served() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    interlaken_telemetry::install_prometheus(([127, 0, 0, 1], port).into()).unwrap();
    metrics::describe_counter!("example_requests_total", "Requests handled.");
    metrics::counter!("example_requests_total", "kind" => "a").increment(3);
    metrics::histogram!("example_duration_seconds").record(0.002);
    metrics::histogram!("example_payload_bytes").record(2000.0);
    let body = scrape(port);
    assert!(
        body.contains("# HELP example_requests_total Requests handled."),
        "{body}"
    );
    assert!(
        body.contains("example_requests_total{kind=\"a\"} 3"),
        "{body}"
    );
    assert!(
        body.contains("example_duration_seconds_bucket{le=\"0.0025\"} 1"),
        "{body}"
    );
    assert!(
        body.contains("example_payload_bytes_bucket{le=\"4096\"} 1"),
        "{body}"
    );
}
