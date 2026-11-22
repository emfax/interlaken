//! Layout of the unversioned `meta` keyspace.

use crate::{Error, Result, Timestamp};

/// On-disk format version written by this build.
///
/// On-disk format stability across versions is out of scope for the MVP
/// (ADR-0001), so a mismatch is refused rather than migrated.
pub const FORMAT_VERSION: u32 = 1;

/// Key holding the format version, as a big-endian `u32`.
pub(crate) const FORMAT_VERSION_KEY: &[u8] = b"format_version";
/// Key holding the last commit timestamp, as a big-endian `u64`.
pub(crate) const LAST_COMMIT_TS_KEY: &[u8] = b"last_commit_ts";
/// Key holding the GC watermark, as a big-endian `u64`.
pub(crate) const GC_WATERMARK_KEY: &[u8] = b"gc_watermark";
/// Key holding the index of the last commit log entry applied to `data`,
/// as a big-endian `u64` (ADR-0037). Absent in databases that never used
/// the commit log, which means 0.
pub(crate) const LAST_APPLIED_INDEX_KEY: &[u8] = b"last_applied_index";

/// Encode a format version as a `meta` value.
pub(crate) fn encode_format_version(version: u32) -> [u8; 4] {
    version.to_be_bytes()
}

/// Decode a format version from a `meta` value.
pub(crate) fn decode_format_version(value: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] = value
        .try_into()
        .map_err(|_| Error::CorruptMeta("format_version is not 4 bytes"))?;
    Ok(u32::from_be_bytes(bytes))
}

/// Encode a timestamp as a `meta` value.
pub(crate) fn encode_timestamp(ts: Timestamp) -> [u8; 8] {
    ts.into_raw().to_be_bytes()
}

/// Decode a timestamp from a `meta` value; `what` names the entry in errors.
pub(crate) fn decode_timestamp(value: &[u8], what: &'static str) -> Result<Timestamp> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| Error::CorruptMeta(what))?;
    Ok(Timestamp::from_raw(u64::from_be_bytes(bytes)))
}
