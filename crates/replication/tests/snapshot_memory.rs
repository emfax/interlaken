//! Snapshots in bounded memory (ADR-0038 phase 3).
//!
//! A counting global allocator (`interlaken-test-alloc`) tracks the heap
//! this process holds. One replica holds live data (64 MiB, or
//! `SNAPSHOT_MEMORY_MIB`) and has compacted its log, so a new replica can
//! only catch up from a snapshot. Streaming it in chunks must keep the
//! process's heap growth below half the snapshot's size, or 64 MiB if that
//! is more: fjall's own block caches, memtables and table metadata take
//! about 40 MiB whatever the size. Sending a snapshot whole held it several
//! times over. The nightly job runs 512 MiB.
//!
//! The test runs alone in its binary, since the counters are process-wide.

use std::time::Duration;

use bytes::Bytes;
use interlaken_replication::{MemoryNetwork, ReplicaConfig, ReplicaNode};
use interlaken_storage::{Durability, Storage, StorageOptions, TxnOptions};
use interlaken_test_alloc::{Counting, held, peak, reset_peak};

#[global_allocator]
/// The process's allocator, counting the heap held.
static ALLOCATOR: Counting = Counting;

/// Return the bytes of live data the first replica holds.
fn data_bytes() -> usize {
    std::env::var("SNAPSHOT_MEMORY_MIB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64)
        << 20
}
/// Bytes in each value.
const VALUE_BYTES: usize = 16 << 10;
/// Values written per transaction.
const VALUES_PER_TXN: usize = 64;

/// Return storage settings with small memtables, so fjall's own buffers stay small.
fn options() -> StorageOptions {
    StorageOptions {
        durability: Durability::Buffered,
        gc_interval: None,
        memtable_bytes: 1 << 20,
        ..StorageOptions::default()
    }
}

/// Return settings for node `id` that compact the log after every few entries.
fn config(id: u64) -> ReplicaConfig {
    let mut config = ReplicaConfig::new(id);
    config.election_min = Duration::from_millis(500);
    config.election_max = Duration::from_millis(1000);
    config.heartbeat = Duration::from_millis(100);
    config.commit.timeout = Duration::from_secs(30);
    config.snapshot_every = 8;
    config.logs_kept = 1;
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Assert a new replica catches up from a snapshot while the heap grows by
/// well under the snapshot's size.
async fn snapshots_stream_in_bounded_memory() {
    let network = MemoryNetwork::new();
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let first = ReplicaNode::open(
        dirs[0].path().to_path_buf(),
        options(),
        config(1),
        network.transport(1),
    )
    .await
    .unwrap();
    network.register(1, first.handler());
    first.initialize(&[1]).await.unwrap();
    while first.leader() != Some(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let value = Bytes::from(vec![7u8; VALUE_BYTES]);
    let count = data_bytes() / VALUE_BYTES;
    for start in (0..count).step_by(VALUES_PER_TXN) {
        let txn = first.storage().begin(TxnOptions::default()).await.unwrap();
        for i in start..(start + VALUES_PER_TXN).min(count) {
            txn.put(Bytes::from(format!("k{i:08}")), value.clone())
                .unwrap();
        }
        txn.commit().await.unwrap();
    }
    for _ in 0..200 {
        if first.status().snapshot_index.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let second = ReplicaNode::open(
        dirs[1].path().to_path_buf(),
        options(),
        config(2),
        network.transport(2),
    )
    .await
    .unwrap();
    network.register(2, second.handler());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let before = held();
    reset_peak();
    first.add_learner(2).await.unwrap();
    let growth = peak().saturating_sub(before);
    assert!(
        second.status().snapshot_index.is_some(),
        "the new replica caught up from a snapshot, not the log"
    );
    assert!(
        second.status().snapshot_index.is_some(),
        "the new replica caught up from a snapshot, not the log"
    );
    eprintln!(
        "a {} MiB snapshot grew the heap by {:.1} MiB at most",
        data_bytes() >> 20,
        growth as f64 / f64::from(1 << 20)
    );
    let bound = (data_bytes() / 2).max(64 << 20);
    assert!(growth < bound, "the heap grew by {growth} bytes");

    let txn = second
        .storage()
        .begin(TxnOptions::default().read_only())
        .await
        .unwrap();
    let last = format!("k{:08}", count - 1);
    let values = txn
        .get_many(vec![Bytes::from_static(b"k00000000"), Bytes::from(last)])
        .await
        .unwrap();
    assert_eq!(values, vec![Some(value.clone()), Some(value)]);
}
