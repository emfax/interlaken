//! Fuzz the schema-less tuple decoder (ADR-0009).
//!
//! Decoding arbitrary bytes must never panic, and any input that decodes
//! successfully must be the canonical encoding of what it decodes to.

#![no_main]

use interlaken_storage::encoding::{KeyBuilder, MAX_TUPLE_LEN, TupleDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(columns) = TupleDecoder::new(data).collect::<Result<Vec<_>, _>>() else {
        return;
    };
    let mut builder = KeyBuilder::with_max_len(MAX_TUPLE_LEN);
    for (datum, order) in &columns {
        builder.push(datum, *order);
    }
    if let Ok(reencoded) = builder.finish() {
        assert_eq!(
            reencoded, data,
            "decoded {columns:?} re-encodes differently"
        );
    }
});
