//! Fuzz the row value decoder (ADR-0009).
//!
//! The first byte picks how many columns the table has (0–15) and the next
//! bytes pick their types; the rest is the stored row. Decoding must never
//! panic, and a row that decodes must be the canonical encoding of its
//! values under the column types it stored.

#![no_main]

use interlaken_storage::encoding::{ColumnType, decode_row, encode_row};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&count, rest)) = data.split_first() else {
        return;
    };
    let count = usize::from(count % 16);
    if rest.len() < count {
        return;
    }
    let (codes, bytes) = rest.split_at(count);
    let types: Vec<ColumnType> = codes
        .iter()
        .map(|c| ColumnType::ALL[usize::from(*c) % ColumnType::ALL.len()])
        .collect();
    let Ok((version, values)) = decode_row(bytes, &types) else {
        return;
    };
    // The row may store fewer columns than the table has; re-encode those.
    let stored = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
    let reencoded = encode_row(version, &types[..stored], &values[..stored])
        .expect("decoded values fit their types");
    assert_eq!(reencoded, bytes, "decoded {values:?} re-encodes differently");
});
