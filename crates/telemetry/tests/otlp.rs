//! Exporting spans over OTLP (ADR-0039 phase 5).

#![cfg(feature = "otlp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

#[test]
/// Assert spans reach a collector as OTLP over HTTP with protobuf, at the
/// latest when the guard is dropped.
fn spans_are_exported() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (heard, received) = mpsc::channel();
    // A collector that answers every request with 200 and reports the
    // request line, content type and body size of each.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let (mut length, mut kind) = (0, String::new());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if let Some(value) = lower.strip_prefix("content-type:") {
                    kind = value.trim().to_owned();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            let _ = heard.send((line.trim_end().to_owned(), kind, body.len()));
        }
    });
    let guard = interlaken_telemetry::install_otlp(
        &format!("http://127.0.0.1:{port}/v1/traces"),
        "interlaken-test",
    )
    .unwrap();
    tracing::info_span!("interlaken.statement", kind = "select").in_scope(|| {});
    drop(guard);
    let (line, kind, size) = received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the collector received the spans");
    assert_eq!(line, "POST /v1/traces HTTP/1.1");
    assert_eq!(kind, "application/x-protobuf");
    assert!(size > 0);
}
