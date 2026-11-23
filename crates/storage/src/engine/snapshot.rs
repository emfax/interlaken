//! The snapshot format (ADR-0038), encoded and decoded in chunks.
//!
//! Encoding (version 2), integers big-endian:
//!
//! ```text
//! snapshot := magic last_commit_ts:u64 meta_count:u32 (bytes bytes)*
//!             (bytes commit_ts:u64 bytes)* end entry_count:u64
//! bytes    := len:u32 byte*
//! ```
//!
//! The `meta` pairs come first, then each key's newest version as key,
//! commit timestamp and value, in key order, then a fixed end marker and
//! the entry count. The trailer makes a snapshot cut short at an entry
//! boundary detectable. A snapshot may be split into chunks anywhere;
//! [`Decoder`] accepts them in order.

use crate::{Error, Result, Timestamp};

/// The first bytes of a snapshot.
pub(crate) const MAGIC: &[u8] = b"ILSNAP2\0";
/// The marker before a snapshot's entry count.
pub(crate) const END: &[u8] = b"ILSNEND\0";

/// Return the corruption error for a malformed snapshot.
pub(crate) fn malformed() -> Error {
    Error::CorruptMeta("malformed snapshot")
}

/// Append `bytes` with its length.
pub(crate) fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(
        &u32::try_from(bytes.len())
            .expect("length fits u32")
            .to_be_bytes(),
    );
    out.extend_from_slice(bytes);
}

/// Where a chunked export resumes.
#[derive(Clone, Debug, Default)]
pub struct ExportCursor {
    /// The last key decided, whose remaining versions are skipped.
    pub(crate) after: Option<Vec<u8>>,
    /// Entries exported so far.
    pub(crate) entries: u64,
    /// Whether the header has been written.
    pub(crate) started: bool,
}

/// Something decoded from a snapshot.
pub(crate) enum Event<'a> {
    /// A `meta` entry.
    Meta(&'a [u8], &'a [u8]),
    /// A key's newest version: key, commit timestamp and value.
    Entry(&'a [u8], Timestamp, &'a [u8]),
}

/// What the decoder expects next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// The magic, last commit timestamp and meta count.
    Header,
    /// This many more `meta` pairs.
    Meta(usize),
    /// Entries, or the trailer.
    Entries,
    /// Nothing: the trailer has been read.
    Done,
}

/// Decodes a snapshot fed in chunks, checking it as it goes.
pub(crate) struct Decoder {
    /// Bytes received but not yet decoded.
    pending: Vec<u8>,
    /// What comes next.
    state: State,
    /// The snapshot's last commit timestamp, once read.
    last: Timestamp,
    /// Entries decoded.
    entries: u64,
}

/// Read a big-endian `u32` length at `at` in `bytes`, if all there.
fn len_at(bytes: &[u8], at: usize) -> Option<usize> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(raw)).ok()
}

/// Read a big-endian `u64` at `at` in `bytes`, if all there.
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_be_bytes(raw))
}

impl Decoder {
    /// Construct a decoder expecting the start of a snapshot.
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            state: State::Header,
            last: Timestamp::ZERO,
            entries: 0,
        }
    }

    /// Decode what `chunk` completes, passing each `meta` entry and version
    /// to `on`, and keep any partial record for the next chunk.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptMeta`] for a malformed snapshot, or `on`'s error.
    pub(crate) fn feed(
        &mut self,
        chunk: &[u8],
        on: &mut dyn FnMut(Event<'_>) -> Result<()>,
    ) -> Result<()> {
        self.pending.extend_from_slice(chunk);
        let bytes = std::mem::take(&mut self.pending);
        let mut at = 0;
        loop {
            match self.state {
                State::Header => {
                    let Some(header) = bytes.get(at..at + MAGIC.len() + 12) else {
                        break;
                    };
                    if !header.starts_with(MAGIC) {
                        return Err(malformed());
                    }
                    self.last =
                        Timestamp::from_raw(u64_at(header, MAGIC.len()).expect("in the header"));
                    let count = len_at(header, MAGIC.len() + 8).expect("in the header");
                    at += header.len();
                    self.state = State::Meta(count);
                }
                State::Meta(0) => self.state = State::Entries,
                State::Meta(left) => {
                    let Some(key_len) = len_at(&bytes, at) else {
                        break;
                    };
                    let Some(value_len) = len_at(&bytes, at + 4 + key_len) else {
                        break;
                    };
                    let end = at + 8 + key_len + value_len;
                    if bytes.len() < end {
                        break;
                    }
                    let key = &bytes[at + 4..at + 4 + key_len];
                    on(Event::Meta(key, &bytes[at + 8 + key_len..end]))?;
                    at = end;
                    self.state = State::Meta(left - 1);
                }
                State::Entries => {
                    // No key starts with the end marker's first four bytes:
                    // as a length, they far exceed any key.
                    if bytes.len() >= at + 4 && bytes[at..].starts_with(&END[..4]) {
                        let Some(count) = u64_at(&bytes, at + END.len()) else {
                            break;
                        };
                        if !bytes[at..].starts_with(END) || count != self.entries {
                            return Err(malformed());
                        }
                        at += END.len() + 8;
                        self.state = State::Done;
                        continue;
                    }
                    let Some(key_len) = len_at(&bytes, at) else {
                        break;
                    };
                    let Some(ts) = u64_at(&bytes, at + 4 + key_len) else {
                        break;
                    };
                    let Some(value_len) = len_at(&bytes, at + 12 + key_len) else {
                        break;
                    };
                    let end = at + 16 + key_len + value_len;
                    if bytes.len() < end {
                        break;
                    }
                    let ts = Timestamp::from_raw(ts);
                    if ts > self.last {
                        return Err(malformed());
                    }
                    on(Event::Entry(
                        &bytes[at + 4..at + 4 + key_len],
                        ts,
                        &bytes[at + 16 + key_len..end],
                    ))?;
                    self.entries += 1;
                    at = end;
                }
                State::Done => {
                    if at < bytes.len() {
                        return Err(malformed());
                    }
                    break;
                }
            }
        }
        self.pending = bytes[at..].to_vec();
        Ok(())
    }

    /// Return the snapshot's last commit timestamp, once all of it has been fed.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptMeta`] if the snapshot ended early.
    pub(crate) fn finish(self) -> Result<Timestamp> {
        if self.state == State::Done && self.pending.is_empty() {
            Ok(self.last)
        } else {
            Err(malformed())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a small snapshot by hand.
    fn sample() -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&9u64.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        put_bytes(&mut out, b"m");
        put_bytes(&mut out, b"v");
        for (key, ts, value) in [(&b"a"[..], 3u64, &b"1"[..]), (b"b", 9, b"")] {
            put_bytes(&mut out, key);
            out.extend_from_slice(&ts.to_be_bytes());
            put_bytes(&mut out, value);
        }
        out.extend_from_slice(END);
        out.extend_from_slice(&2u64.to_be_bytes());
        out
    }

    /// Decode `chunks` in order, returning the events and the result.
    fn decode(chunks: &[&[u8]]) -> (Vec<String>, Result<Timestamp>) {
        let mut seen = Vec::new();
        let mut decoder = Decoder::new();
        for chunk in chunks {
            let fed = decoder.feed(chunk, &mut |event| {
                seen.push(match event {
                    Event::Meta(k, v) => format!("meta {k:?}={v:?}"),
                    Event::Entry(k, ts, v) => format!("entry {k:?}@{}={v:?}", ts.into_raw()),
                });
                Ok(())
            });
            if let Err(e) = fed {
                return (seen, Err(e));
            }
        }
        (seen, decoder.finish())
    }

    #[test]
    /// Assert a snapshot decodes the same however it is split into chunks.
    fn chunks_split_anywhere() {
        let bytes = sample();
        let (whole, last) = decode(&[&bytes]);
        assert_eq!(last.unwrap(), Timestamp::from_raw(9));
        assert_eq!(whole.len(), 3);
        for split in 0..=bytes.len() {
            let (seen, last) = decode(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(seen, whole, "split at {split}");
            assert_eq!(last.unwrap(), Timestamp::from_raw(9));
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(decode(&singles).0, whole);
    }

    #[test]
    /// Assert truncated, extended and inconsistent snapshots are refused.
    fn damage_is_refused() {
        let bytes = sample();
        for len in 0..bytes.len() {
            assert!(decode(&[&bytes[..len]]).1.is_err(), "prefix of {len}");
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode(&[&extra]).1.is_err());
        let mut wrong_count = bytes.clone();
        let at = wrong_count.len() - 1;
        wrong_count[at] = 3;
        assert!(decode(&[&wrong_count]).1.is_err());
    }
}
