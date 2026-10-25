//! A per-thread scratch buffer for the `/proc` readers' one-call reads,
//! `getdents` listings and memory-map passes, whose content is parsed or
//! copied out (at its own length) before the buffer is used again.
//!
//! Why: the in-use scan reads every task on [`crate::host`]'s reader
//! threads, and each read used to allocate its own buffer of 16 KiB to
//! 128 KiB, growing. The shipped static build's allocator (musl's
//! mallocng) serializes every thread on one lock and serves and frees
//! buffers of that size with `mmap`/`munmap`, so on a busy host the
//! readers wait on each other (TASK-145; the measurements are in its
//! commit). A scan's wall time is what the
//! Freshness bound and the 500 ms reuse limit are measured against
//! (docs/safety.md, In use), so it decides how many scans one Cargo
//! target's parts cost. What is read does not change: a caller still sizes
//! its own call, and uses only what its own call wrote.
//!
//! The buffer is kept for the thread's life only while its capacity is at
//! most [`RETAINED_MAX`]; a larger one (a rare huge fd table, environment
//! or memory map) is freed after its use, so what a thread keeps is
//! bounded. A nested use on the same thread (none today) gets a buffer of
//! its own rather than a panic.

use std::cell::RefCell;

/// The largest capacity a thread keeps between uses: the usual sizes (32
/// and 64 KiB listings and environments, memory maps of some 10^5 bytes),
/// not the 16 MiB or 64 MiB bounds of the rarest reads.
pub const RETAINED_MAX: usize = 1024 * 1024;

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Calls `use_buffer` with this thread's scratch buffer, empty (length 0,
/// its capacity kept from earlier uses).
pub fn with_scratch<R>(use_buffer: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buffer) => {
            buffer.clear();
            let result = use_buffer(&mut buffer);
            if buffer.capacity() > RETAINED_MAX {
                *buffer = Vec::new();
            }
            result
        }
        Err(_) => use_buffer(&mut Vec::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_is_empty_on_entry_and_its_capacity_is_reused() {
        let first = with_scratch(|buffer| {
            assert!(buffer.is_empty());
            buffer.resize(64, 7);
            buffer.as_ptr() as usize
        });
        let second = with_scratch(|buffer| {
            assert!(buffer.is_empty(), "nothing of the last use is visible");
            assert!(buffer.capacity() >= 64);
            buffer.as_ptr() as usize
        });
        assert_eq!(first, second, "the thread's buffer is reused");
    }

    #[test]
    fn a_nested_use_gets_its_own_buffer() {
        with_scratch(|outer| {
            outer.reserve(8);
            let outer_at = outer.as_ptr() as usize;
            with_scratch(|inner| {
                inner.reserve(8);
                assert_ne!(inner.as_ptr() as usize, outer_at);
            });
        });
    }

    #[test]
    fn a_buffer_larger_than_the_retained_bound_is_freed_after_use() {
        with_scratch(|buffer| buffer.reserve(RETAINED_MAX + 1));
        SCRATCH.with(|cell| assert_eq!(cell.borrow().capacity(), 0));
        with_scratch(|buffer| buffer.reserve(RETAINED_MAX));
        SCRATCH.with(|cell| assert!(cell.borrow().capacity() >= RETAINED_MAX));
    }
}
