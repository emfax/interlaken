//! Fuzz versioned key splitting (ADR-0009).
//!
//! Splitting must never panic, and re-joining the parts must reproduce the input.

#![no_main]

use interlaken_storage::encoding::{split_versioned_key, versioned_key};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((tuple, ts)) = split_versioned_key(data) {
        assert_eq!(versioned_key(tuple, ts), data);
    }
});
