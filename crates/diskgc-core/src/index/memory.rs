//! How the index counts its own memory (docs/architecture.md, Memory
//! budget and collapse).
//!
//! The count is an **upper bound of the heap the index owns**, computed
//! from the shapes of its containers, never sampled from the allocator (the
//! core makes no syscalls and must be deterministic):
//!
//! - every allocation counts its requested size plus [`ALLOC_OVERHEAD_BYTES`]
//!   for the allocator's own header and rounding;
//! - a `Vec` or boxed slice counts its capacity, not its length;
//! - a `BTreeMap` (the trie's children) counts the most nodes it can have
//!   for its length: the standard library keeps every node but the root at
//!   least `B - 1 = 5` of its 11 slots full, so `len` entries need at most
//!   `1 + (len - 1) / 5` nodes, each counted at the size of an internal
//!   node ([`btree_node_bytes`], an upper bound of the library's layout).
//!
//! The bound is checked against the allocator by the counting-allocator
//! test in diskgc-model (`tests/index_memory.rs`): the bytes allocated for
//! an index, plus the per-allocation overhead, never exceed this count. The
//! tally's live cursor is counted too, by the tally
//! ([`super::Index::set_cursor_bytes`], [`crate::tally::size::Cursor`]);
//! transient memory of a collapse, a queue derivation or one tally step
//! (bounded working sets: listings, open directories) is not.

use std::mem::size_of;

/// The index's budget: a ceiling, never exceeded (64 MB, as
/// docs/architecture.md says; a constant, not configurable in v1).
pub const BUDGET_BYTES: usize = 64_000_000;

/// After a collapse the index is at or below this share of its budget, in
/// percent (the low-water mark).
pub const LOW_WATER_PERCENT: usize = 75;

/// Counted per allocation on top of its size: glibc's malloc keeps an
/// 8-byte header and rounds chunks to 16 bytes; musl's mallocng is in the
/// same range. An estimate, not a measurement of either.
pub const ALLOC_OVERHEAD_BYTES: usize = 16;

/// `B = 6` in the standard library's B-tree: 11 slots per node, every
/// non-root node at least 5 full.
const BTREE_CAPACITY: usize = 11;
const BTREE_MIN_LEN: usize = 5;

/// One allocation of `bytes` (nothing for zero: no allocation is made).
pub const fn heap(bytes: usize) -> usize {
    if bytes == 0 {
        0
    } else {
        bytes + ALLOC_OVERHEAD_BYTES
    }
}

/// A `Vec<T>` of `capacity` (or a boxed slice of that length).
pub const fn vec_bytes(capacity: usize, elem: usize) -> usize {
    heap(capacity * elem)
}

/// An upper bound of the size of an internal B-tree node with keys of
/// `key` and values of `value` bytes: parent pointer, index and length,
/// the key and value arrays and 12 child pointers, rounded up to 16.
pub const fn btree_node_bytes(key: usize, value: usize) -> usize {
    let raw = 16 + BTREE_CAPACITY * (key + value) + (BTREE_CAPACITY + 1) * size_of::<usize>();
    raw.div_ceil(16) * 16
}

/// An upper bound of the heap of a `BTreeMap` of `len` entries whose
/// internal nodes are at most `node_bytes`.
pub const fn btree_bytes(len: usize, node_bytes: usize) -> usize {
    if len == 0 {
        0
    } else {
        (1 + (len - 1) / BTREE_MIN_LEN) * heap(node_bytes)
    }
}

/// The low-water mark of `budget`.
pub const fn low_water(budget: usize) -> usize {
    budget / 100 * LOW_WATER_PERCENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btree_bound_grows_by_one_node_per_five_entries() {
        let node = btree_node_bytes(16, 56);
        assert_eq!(btree_bytes(0, node), 0);
        assert_eq!(btree_bytes(1, node), heap(node));
        assert_eq!(btree_bytes(6, node), 2 * heap(node));
        assert_eq!(btree_bytes(11, node), 3 * heap(node));
    }

    #[test]
    fn marks_of_the_real_budget() {
        assert_eq!(low_water(BUDGET_BYTES), 48_000_000);
    }
}
