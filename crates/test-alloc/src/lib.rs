//! A counting global allocator for memory-accounting tests.
//!
//! [`Counting`] forwards every call to the system allocator and counts the
//! heap the process holds, in the chunks glibc's `malloc` hands out (see
//! [`chunk`]), since that is what the resident set pays for. A test binary
//! installs it with
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: interlaken_test_alloc::Counting = interlaken_test_alloc::Counting;
//! ```
//!
//! and reads [`held`] and [`peak`], restarting the peak with
//! [`reset_peak`]. The counters are process-wide, so a test measuring them
//! must not run alongside other tests that allocate.
//!
//! This is the workspace's only `unsafe` code: implementing
//! [`GlobalAlloc`] requires it. It is a dev-dependency only.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Requests of at least this many bytes are mapped in whole pages.
const MMAP_THRESHOLD: usize = 128 * 1024;

/// Heap bytes currently held.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// The most heap bytes held since the last [`reset_peak`].
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// A global allocator that counts the heap held, in allocator chunks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counting;

/// Return the bytes glibc's `malloc` hands out for a request of `n` bytes.
///
/// A chunk is the request plus an 8-byte header, rounded up to 16 bytes,
/// and at least 32; requests of 128 KiB or more are mapped in whole pages.
///
/// # Examples
///
/// ```
/// assert_eq!(interlaken_test_alloc::chunk(13), 32);
/// assert_eq!(interlaken_test_alloc::chunk(100), 112);
/// ```
#[must_use]
pub fn chunk(n: usize) -> usize {
    if n >= MMAP_THRESHOLD {
        (n + 16).next_multiple_of(4096)
    } else {
        (n + 8).next_multiple_of(16).max(32)
    }
}

/// Return the heap bytes currently held, if [`Counting`] is the global allocator.
#[must_use]
pub fn held() -> usize {
    HELD.load(Ordering::Relaxed)
}

/// Return the most heap bytes held since the last [`reset_peak`].
#[must_use]
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Restart peak tracking from the heap held now; return it.
pub fn reset_peak() -> usize {
    let held = held();
    PEAK.store(held, Ordering::Relaxed);
    held
}

/// Record `bytes` more held, raising the peak.
fn add(bytes: usize) {
    let held = HELD.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(held, Ordering::Relaxed);
}

/// Record `bytes` fewer held.
fn sub(bytes: usize) {
    HELD.fetch_sub(bytes, Ordering::Relaxed);
}

// SAFETY: every call is forwarded unchanged to the system allocator, which
// meets `GlobalAlloc`'s contract; only counters are updated besides.
unsafe impl GlobalAlloc for Counting {
    /// Allocate through the system allocator, counting the chunk.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(chunk(layout.size()));
        }
        p
    }

    /// Allocate zeroed memory through the system allocator, counting the chunk.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(chunk(layout.size()));
        }
        p
    }

    /// Free through the system allocator, uncounting the chunk.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        sub(chunk(layout.size()));
    }

    /// Reallocate through the system allocator, counting the new chunk before uncounting the old.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            add(chunk(new_size));
            sub(chunk(layout.size()));
        }
        p
    }
}
