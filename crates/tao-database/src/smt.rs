//! Sparse Merkle Tree over the 256-bit account keyspace.
//!
//! Commits the account set to a single 256-bit root and lets a light client
//! prove an account's value (inclusion) or its absence (exclusion) against that
//! root without the full state. Standard construction: a binary Merkle tree of
//! height 256 keyed by the account pubkey's bits (MSB first), with empty
//! subtrees collapsed to precomputed per-level *default* hashes (sparsity).
//!
//! This module computes the root and proofs from the current key→value-hash set
//! (a deterministic function of the accounts, so all nodes agree). Maintaining it
//! incrementally in storage (O(log n) per write, O(1) root) is a follow-on; the
//! root and proofs here are the consensus/light-client value.

/// Tree height = key bit-length.
pub const HEIGHT: usize = 256;

/// Hash of an internal node from its two children (domain-separated).
fn hash2(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x01]);
    h.update(l);
    h.update(r);
    *h.finalize().as_bytes()
}

/// Hash of a leaf binding the key to its value hash (domain-separated).
pub fn leaf_hash(key: &[u8; 32], value_hash: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x00]);
    h.update(key);
    h.update(value_hash);
    *h.finalize().as_bytes()
}

/// The empty-leaf placeholder (value of an absent key).
pub const EMPTY_LEAF: [u8; 32] = [0u8; 32];

/// Per-level default hashes: `defaults[HEIGHT]` is the empty leaf, and
/// `defaults[i] = hash2(defaults[i+1], defaults[i+1])` — the root of an all-empty
/// subtree rooted at level `i`.
fn defaults() -> [[u8; 32]; HEIGHT + 1] {
    let mut d = [[0u8; 32]; HEIGHT + 1];
    d[HEIGHT] = EMPTY_LEAF;
    let mut i = HEIGHT;
    while i > 0 {
        d[i - 1] = hash2(&d[i], &d[i]);
        i -= 1;
    }
    d
}

/// Bit `depth` of `key`, MSB first (depth 0 is the most significant bit).
fn bit(key: &[u8; 32], depth: usize) -> u8 {
    (key[depth / 8] >> (7 - (depth % 8))) & 1
}

/// An inclusion/exclusion proof: the sibling hash at each level, top (level 0)
/// to bottom (level 255).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleProof {
    pub siblings: Vec<[u8; 32]>,
}

/// A sparse Merkle tree built from a key → value-hash set.
pub struct SparseMerkleTree {
    /// Sorted (key, leaf_hash) entries.
    leaves: Vec<([u8; 32], [u8; 32])>,
    defaults: [[u8; 32]; HEIGHT + 1],
}

impl SparseMerkleTree {
    /// Build from `(key, value_hash)` pairs. Duplicate keys keep the last value.
    pub fn from_entries(mut entries: Vec<([u8; 32], [u8; 32])>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0); // keeps the first of equal keys after sort; callers shouldn't dup
        let leaves = entries
            .into_iter()
            .map(|(k, v)| (k, leaf_hash(&k, &v)))
            .collect();
        Self { leaves, defaults: defaults() }
    }

    /// The 256-bit state root (the all-empty tree hashes to `defaults[0]`).
    pub fn root(&self) -> [u8; 32] {
        self.subtree_root(&self.leaves, 0)
    }

    /// Root of the subtree at `depth` covering the sorted `leaves` slice.
    fn subtree_root(&self, leaves: &[([u8; 32], [u8; 32])], depth: usize) -> [u8; 32] {
        if leaves.is_empty() {
            return self.defaults[depth];
        }
        if depth == HEIGHT {
            // Exactly one key reached full depth.
            return leaves[0].1;
        }
        let split = leaves.partition_point(|(k, _)| bit(k, depth) == 0);
        let (left, right) = leaves.split_at(split);
        hash2(
            &self.subtree_root(left, depth + 1),
            &self.subtree_root(right, depth + 1),
        )
    }

    /// Inclusion proof for `key` (the 256 sibling hashes along its path). Works
    /// whether or not `key` is present (an absent key yields an exclusion proof,
    /// verified against `EMPTY_LEAF`).
    pub fn proof(&self, key: &[u8; 32]) -> MerkleProof {
        let mut siblings = Vec::with_capacity(HEIGHT);
        self.collect_siblings(&self.leaves, key, 0, &mut siblings);
        MerkleProof { siblings }
    }

    fn collect_siblings(
        &self,
        leaves: &[([u8; 32], [u8; 32])],
        key: &[u8; 32],
        depth: usize,
        out: &mut Vec<[u8; 32]>,
    ) {
        if depth == HEIGHT {
            return;
        }
        let split = leaves.partition_point(|(k, _)| bit(k, depth) == 0);
        let (left, right) = leaves.split_at(split);
        if bit(key, depth) == 0 {
            out.push(self.subtree_root(right, depth + 1));
            self.collect_siblings(left, key, depth + 1, out);
        } else {
            out.push(self.subtree_root(left, depth + 1));
            self.collect_siblings(right, key, depth + 1, out);
        }
    }
}

/// Verify a proof: fold the `leaf` up through the `siblings` (bottom to top) and
/// check the result equals `root`. For inclusion pass `leaf_hash(key, value)`;
/// for exclusion pass [`EMPTY_LEAF`].
pub fn verify(root: &[u8; 32], key: &[u8; 32], leaf: &[u8; 32], proof: &MerkleProof) -> bool {
    if proof.siblings.len() != HEIGHT {
        return false;
    }
    let mut cur = *leaf;
    for depth in (0..HEIGHT).rev() {
        let sib = &proof.siblings[depth];
        cur = if bit(key, depth) == 0 {
            hash2(&cur, sib)
        } else {
            hash2(sib, &cur)
        };
    }
    &cur == root
}

// ---------------------------------------------------------------------------
// Incremental / persistent variant: store only non-default nodes in a key-value
// store, updating O(HEIGHT) nodes per leaf change. Root is an O(1) lookup. The
// result is identical to the from-scratch [`SparseMerkleTree`] for the same set.
// ---------------------------------------------------------------------------

/// A key-value store of SMT nodes (node id → 32-byte hash). Absence = the
/// per-level default (empty subtree) hash.
pub trait NodeStore {
    fn get_node(&self, id: &[u8]) -> Option<[u8; 32]>;
    fn set_node(&mut self, id: &[u8], hash: [u8; 32]);
    fn del_node(&mut self, id: &[u8]);
}

/// Per-level default hashes (public accessor).
pub fn default_hashes() -> [[u8; 32]; HEIGHT + 1] {
    defaults()
}

/// Canonical id of the node at `depth` on `key`'s path: `[depth:u16][first `depth`
/// bits of key, MSB-first, trailing bits of the last byte masked to zero]`.
pub fn node_id(key: &[u8; 32], depth: usize) -> Vec<u8> {
    let nbytes = depth.div_ceil(8);
    let mut id = Vec::with_capacity(2 + nbytes);
    id.extend_from_slice(&(depth as u16).to_be_bytes());
    for i in 0..nbytes {
        let mut b = key[i];
        if i == nbytes - 1 {
            let used = depth - i * 8; // 1..=8 significant bits in the last byte
            if used < 8 {
                b &= !((1u8 << (8 - used)) - 1);
            }
        }
        id.push(b);
    }
    id
}

fn flip_bit(key: &[u8; 32], depth: usize) -> [u8; 32] {
    let mut k = *key;
    k[depth / 8] ^= 1 << (7 - (depth % 8));
    k
}

/// Set the leaf for `key` to `leaf` (pass [`EMPTY_LEAF`] to remove it) and
/// recompute the path to the root, writing only non-default nodes.
pub fn update_leaf<S: NodeStore>(store: &mut S, key: &[u8; 32], leaf: [u8; 32]) {
    let d = defaults();
    let mut cur_hash = leaf;
    let mut cur_id = node_id(key, HEIGHT);
    if cur_hash == d[HEIGHT] {
        store.del_node(&cur_id);
    } else {
        store.set_node(&cur_id, cur_hash);
    }
    let mut depth = HEIGHT;
    while depth > 0 {
        let sib_id = node_id(&flip_bit(key, depth - 1), depth);
        let sib = store.get_node(&sib_id).unwrap_or(d[depth]);
        let (l, r) = if bit(key, depth - 1) == 0 { (cur_hash, sib) } else { (sib, cur_hash) };
        let parent = hash2(&l, &r);
        depth -= 1;
        cur_hash = parent;
        cur_id = node_id(key, depth);
        if cur_hash == d[depth] {
            store.del_node(&cur_id);
        } else {
            store.set_node(&cur_id, cur_hash);
        }
    }
}

/// The root from a node store (O(1) lookup).
pub fn stored_root<S: NodeStore>(store: &S) -> [u8; 32] {
    store.get_node(&node_id(&[0u8; 32], 0)).unwrap_or_else(|| defaults()[0])
}

/// Inclusion/exclusion proof for `key` read from a node store.
pub fn stored_proof<S: NodeStore>(store: &S, key: &[u8; 32]) -> MerkleProof {
    let d = defaults();
    let mut siblings = Vec::with_capacity(HEIGHT);
    for depth in 0..HEIGHT {
        let sib_id = node_id(&flip_bit(key, depth), depth + 1);
        siblings.push(store.get_node(&sib_id).unwrap_or(d[depth + 1]));
    }
    MerkleProof { siblings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory NodeStore for testing the incremental variant.
    #[derive(Default)]
    struct MemStore(HashMap<Vec<u8>, [u8; 32]>);
    impl NodeStore for MemStore {
        fn get_node(&self, id: &[u8]) -> Option<[u8; 32]> {
            self.0.get(id).copied()
        }
        fn set_node(&mut self, id: &[u8], hash: [u8; 32]) {
            self.0.insert(id.to_vec(), hash);
        }
        fn del_node(&mut self, id: &[u8]) {
            self.0.remove(id);
        }
    }

    fn k(b: u8) -> [u8; 32] {
        let mut x = [0u8; 32];
        x[0] = b;
        x
    }
    fn vh(b: u8) -> [u8; 32] {
        let mut x = [b; 32];
        x[31] = b;
        x
    }

    #[test]
    fn empty_root_is_default() {
        let t = SparseMerkleTree::from_entries(vec![]);
        assert_eq!(t.root(), defaults()[0]);
    }

    #[test]
    fn root_is_deterministic_and_order_independent() {
        let a = SparseMerkleTree::from_entries(vec![(k(1), vh(1)), (k(2), vh(2)), (k(200), vh(3))]);
        let b = SparseMerkleTree::from_entries(vec![(k(200), vh(3)), (k(1), vh(1)), (k(2), vh(2))]);
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn changing_a_value_changes_the_root() {
        let a = SparseMerkleTree::from_entries(vec![(k(1), vh(1)), (k(2), vh(2))]);
        let b = SparseMerkleTree::from_entries(vec![(k(1), vh(9)), (k(2), vh(2))]);
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn inclusion_proof_verifies() {
        let entries = vec![(k(1), vh(1)), (k(2), vh(2)), (k(128), vh(3)), (k(255), vh(4))];
        let t = SparseMerkleTree::from_entries(entries.clone());
        let root = t.root();
        for (key, value) in &entries {
            let proof = t.proof(key);
            assert!(verify(&root, key, &leaf_hash(key, value), &proof), "key {:?}", key[0]);
            // Wrong value fails.
            assert!(!verify(&root, key, &leaf_hash(key, &vh(77)), &proof));
        }
    }

    #[test]
    fn exclusion_proof_verifies() {
        let t = SparseMerkleTree::from_entries(vec![(k(1), vh(1)), (k(2), vh(2))]);
        let root = t.root();
        let absent = k(50);
        let proof = t.proof(&absent);
        // Absent key: the leaf slot holds EMPTY_LEAF.
        assert!(verify(&root, &absent, &EMPTY_LEAF, &proof), "exclusion proof");
        // It is NOT present with any value.
        assert!(!verify(&root, &absent, &leaf_hash(&absent, &vh(1)), &proof));
    }

    #[test]
    fn incremental_root_matches_from_scratch() {
        // The persistent/incremental store must produce the same root and proofs
        // as the from-scratch tree for the same account set, through inserts,
        // updates, and deletes.
        let mut store = MemStore::default();
        let mut set: Vec<([u8; 32], [u8; 32])> = Vec::new();

        let ops: &[([u8; 32], Option<[u8; 32]>)] = &[
            (k(1), Some(vh(1))),
            (k(200), Some(vh(2))),
            (k(2), Some(vh(3))),
            (k(1), Some(vh(9))),  // update
            (k(128), Some(vh(4))),
            (k(200), None),       // delete
        ];
        for (key, val) in ops {
            match val {
                Some(v) => {
                    update_leaf(&mut store, key, leaf_hash(key, v));
                    set.retain(|(kk, _)| kk != key);
                    set.push((*key, *v));
                }
                None => {
                    update_leaf(&mut store, key, EMPTY_LEAF);
                    set.retain(|(kk, _)| kk != key);
                }
            }
            let reference = SparseMerkleTree::from_entries(set.clone()).root();
            assert_eq!(stored_root(&store), reference, "incremental root diverged");
        }

        // A proof read from the store verifies against the stored root.
        let root = stored_root(&store);
        let proof = stored_proof(&store, &k(1));
        assert!(verify(&root, &k(1), &leaf_hash(&k(1), &vh(9)), &proof));
        // Deleted key proves absent.
        let xproof = stored_proof(&store, &k(200));
        assert!(verify(&root, &k(200), &EMPTY_LEAF, &xproof));
    }

    #[test]
    fn single_key_round_trips() {
        let t = SparseMerkleTree::from_entries(vec![(k(42), vh(7))]);
        let root = t.root();
        assert_ne!(root, defaults()[0]);
        let proof = t.proof(&k(42));
        assert!(verify(&root, &k(42), &leaf_hash(&k(42), &vh(7)), &proof));
    }
}
