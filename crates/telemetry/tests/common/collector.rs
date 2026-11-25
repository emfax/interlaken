//! A stand-in OTLP collector for the exporter tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// One request the collector received.
#[derive(Debug)]
pub struct Received {
    /// The request line, such as `POST /v1/traces HTTP/1.1`.
    pub line: String,
    /// The `content-type` header, lowercased.
    pub kind: String,
    /// The body's size in bytes.
    pub size: usize,
}

/// Start a collector that answers every request with 200, and return its
/// port and a channel reporting each request it received.
pub fn start() -> (u16, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (heard, received) = mpsc::channel();
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
            let _ = heard.send(Received {
                line: line.trim_end().to_owned(),
                kind,
                size: body.len(),
            });
        }
    });
    (port, received)
}
