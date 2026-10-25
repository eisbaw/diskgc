//! The per-mount path trie: one [`Node`] per path component below the
//! mount point that leads to a unit. A node holds at most one [`Single`]
//! (the candidate at that path) and the [`Block`]s sited at that
//! directory; a node with neither and no children is removed.
//!
//! Every function that changes a trie returns the change of the counted
//! memory and of the unit count as a [`Delta`], so the index keeps its
//! count without a walk ([`super::memory`] says what is counted).
//! [`Node::subtree_cost`] counts the same from scratch, which tests compare.

use std::collections::BTreeMap;
use std::mem::size_of;

use super::memory::{btree_bytes, btree_node_bytes, heap, vec_bytes};
use super::unit::{Block, Family, Single};

pub(super) type Name = Box<[u8]>;

/// A path component's node.
#[derive(Debug, Default)]
pub(super) struct Node {
    pub(super) children: BTreeMap<Name, Node>,
    pub(super) single: Option<Box<Single>>,
    pub(super) blocks: Vec<Block>,
}

/// The internal-node bound of a children map.
const CHILDREN_NODE_BYTES: usize = btree_node_bytes(size_of::<Name>(), size_of::<Node>());

/// A change of counted bytes and of units (singles plus blocks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Delta {
    pub(super) bytes: isize,
    pub(super) units: isize,
}

impl Delta {
    pub(super) fn add(&mut self, other: Delta) {
        self.bytes = self.bytes.checked_add(other.bytes).expect("delta bytes");
        self.units = self.units.checked_add(other.units).expect("delta units");
    }
}

fn signed(bytes: usize) -> isize {
    isize::try_from(bytes).expect("index bytes fit isize")
}

pub(super) fn children_map_bytes(len: usize) -> usize {
    btree_bytes(len, CHILDREN_NODE_BYTES)
}

pub(super) fn name_bytes(name: &[u8]) -> usize {
    heap(name.len())
}

pub(super) fn single_bytes() -> usize {
    heap(size_of::<Single>())
}

pub(super) fn family_bytes(family: &Family) -> usize {
    match family {
        Family::Name(name) => heap(name.len()),
        Family::Any => 0,
    }
}

fn blocks_bytes(blocks: &Vec<Block>) -> usize {
    vec_bytes(blocks.capacity(), size_of::<Block>())
        + blocks
            .iter()
            .map(|b| family_bytes(&b.family))
            .sum::<usize>()
}

/// What a block vector holding exactly `blocks` costs (capacity kept at the
/// length, [`Node::change_blocks`]).
pub(super) fn exact_blocks_bytes<'b>(blocks: impl IntoIterator<Item = &'b Block>) -> usize {
    let (count, families) = blocks
        .into_iter()
        .fold((0, 0), |(n, f), b| (n + 1, f + family_bytes(&b.family)));
    vec_bytes(count, size_of::<Block>()) + families
}

/// What adding a child named `name` to a map of `len` entries costs.
pub(super) fn new_child_bytes(len: usize, name: &[u8]) -> usize {
    children_map_bytes(len + 1) - children_map_bytes(len) + name_bytes(name)
}

impl Node {
    pub(super) fn is_empty(&self) -> bool {
        self.children.is_empty() && self.single.is_none() && self.blocks.is_empty()
    }

    /// Bytes this node owns outside its parent's map, children excluded.
    fn own_bytes(&self) -> usize {
        children_map_bytes(self.children.len())
            + self.children.keys().map(|k| name_bytes(k)).sum::<usize>()
            + self.single.as_ref().map_or(0, |_| single_bytes())
            + blocks_bytes(&self.blocks)
    }

    fn own_units(&self) -> usize {
        usize::from(self.single.is_some()) + self.blocks.len()
    }

    /// Counted bytes and units of this node and everything below, from
    /// scratch (the parent's map slot and the node's name not included).
    pub(super) fn subtree_cost(&self) -> (usize, usize) {
        let mut bytes = self.own_bytes();
        let mut units = self.own_units();
        for child in self.children.values() {
            let (b, u) = child.subtree_cost();
            bytes += b;
            units += u;
        }
        (bytes, units)
    }

    /// The child `name`, created if missing.
    pub(super) fn child_entry(&mut self, name: &[u8], delta: &mut Delta) -> &mut Node {
        if !self.children.contains_key(name) {
            let bytes = new_child_bytes(self.children.len(), name);
            self.children.insert(name.into(), Node::default());
            delta.add(Delta {
                bytes: signed(bytes),
                units: 0,
            });
        }
        self.children.get_mut(name).expect("just ensured")
    }

    /// Removes the child `name` and everything below it.
    pub(super) fn remove_child(&mut self, name: &[u8], delta: &mut Delta) {
        let len = self.children.len();
        let Some(child) = self.children.remove(name) else {
            return;
        };
        let (bytes, units) = child.subtree_cost();
        let freed =
            bytes + name_bytes(name) + children_map_bytes(len) - children_map_bytes(len - 1);
        delta.add(Delta {
            bytes: -signed(freed),
            units: -signed(units),
        });
    }

    /// Removes the child `name` if nothing is left in it.
    pub(super) fn prune_child(&mut self, name: &[u8], delta: &mut Delta) {
        if self.children.get(name).is_some_and(Node::is_empty) {
            self.remove_child(name, delta);
        }
    }

    pub(super) fn set_single(&mut self, single: Single, delta: &mut Delta) {
        if self.single.is_none() {
            delta.add(Delta {
                bytes: signed(single_bytes()),
                units: 1,
            });
        }
        self.single = Some(Box::new(single));
    }

    pub(super) fn take_single(&mut self, delta: &mut Delta) -> Option<Single> {
        let single = self.single.take()?;
        delta.add(Delta {
            bytes: -signed(single_bytes()),
            units: -1,
        });
        Some(*single)
    }

    /// Replaces the blocks with what `change` leaves, counting the
    /// difference. The vector is kept at its exact length.
    pub(super) fn change_blocks(
        &mut self,
        delta: &mut Delta,
        change: impl FnOnce(&mut Vec<Block>),
    ) {
        let before = (blocks_bytes(&self.blocks), self.blocks.len());
        change(&mut self.blocks);
        self.blocks.shrink_to_fit();
        let after = (blocks_bytes(&self.blocks), self.blocks.len());
        delta.add(Delta {
            bytes: signed(after.0) - signed(before.0),
            units: signed(after.1) - signed(before.1),
        });
    }

    /// What removing the block at `at` frees ([`Self::change_blocks`]
    /// keeps the capacity at the length).
    pub(super) fn remove_block_bytes(&self, at: usize) -> usize {
        assert_eq!(
            self.blocks.capacity(),
            self.blocks.len(),
            "blocks kept exact"
        );
        let len = self.blocks.len();
        vec_bytes(len, size_of::<Block>()) - vec_bytes(len - 1, size_of::<Block>())
            + family_bytes(&self.blocks[at].family)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_deltas_equal_a_count_from_scratch() {
        let mut root = Node::default();
        let mut delta = Delta::default();
        for name in [&b"a"[..], b"bb", b"ccc", b"d", b"e", b"f", b"g"] {
            root.child_entry(name, &mut delta)
                .child_entry(b"x", &mut delta);
        }
        assert_eq!(signed(root.subtree_cost().0), delta.bytes);
        root.remove_child(b"bb", &mut delta);
        root.remove_child(b"zz", &mut delta);
        assert_eq!(signed(root.subtree_cost().0), delta.bytes);
        assert_eq!(delta.units, 0);
    }
}
