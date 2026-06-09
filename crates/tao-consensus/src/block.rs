//! Block and block-header types.
//!
//! The header is what PoW grinds over and what the chain commits to. In the
//! launch (linear PoW) phase a block has a single parent (`prev_hash`); the
//! `blockDAG` upgrade (M8) generalizes this to multiple parents.

use serde::{Deserialize, Serialize};
use tao_core::{Hash, Pubkey};

use crate::target::Target;

/// The current header version.
pub const HEADER_VERSION: u32 = 1;

/// A 32-byte block identifier (the hash of the header).
pub type BlockId = [u8; 32];

/// Block header — the unit of PoW and chain commitment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    /// Header format version (enables soft/hard-fork evolution).
    pub version: u32,
    /// Hash of the parent block's header. All-zero for genesis.
    pub prev_hash: Hash,
    /// Height above genesis (genesis = 0).
    pub height: u64,
    /// Block timestamp (unix seconds).
    pub timestamp: i64,
    /// Merkle root over the block's transactions.
    pub tx_merkle_root: Hash,
    /// Commitment to post-execution account state. Zero until M2 wires the SVM.
    pub state_root: Hash,
    /// PoW target threshold (big-endian). `pow_hash <= target` wins.
    pub target: Target,
    /// PoW solution nonce. This is the `pow_proof` for the RandomX/Blake3 phase;
    /// the matmul-PoUW phase (M7) replaces/augments it with a STARK proof.
    pub nonce: u64,
    /// Address that receives this block's coinbase reward.
    pub miner: Pubkey,
}

impl BlockHeader {
    /// Serialize the header deterministically (bincode) for hashing and storage.
    pub fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).expect("header serialization is infallible")
    }

    /// The block id: BLAKE3 of the serialized header (includes the nonce).
    ///
    /// Note: this is the block *identity* hash. The PoW hash is computed by the
    /// active [`crate::pow::PowAlgorithm`] and is only the same as this for the
    /// Blake3 algorithm.
    pub fn id(&self) -> BlockId {
        *blake3::hash(&self.serialize()).as_bytes()
    }

    /// Is this the genesis header?
    pub fn is_genesis(&self) -> bool {
        self.height == 0 && self.prev_hash == Hash::default()
    }
}

/// A full block: header plus transactions.
///
/// For M1 transactions are opaque byte blobs (empty blocks + a coinbase). M2
/// replaces the payload with sanitized Solana transactions while keeping the
/// wire form as bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    /// Serialized transactions (coinbase first, by convention).
    pub transactions: Vec<Vec<u8>>,
}

impl Block {
    pub fn new(header: BlockHeader, transactions: Vec<Vec<u8>>) -> Self {
        Self { header, transactions }
    }

    pub fn id(&self) -> BlockId {
        self.header.id()
    }
}

/// Compute a transaction Merkle root (BLAKE3) over serialized transactions.
///
/// Empty blocks hash to a fixed all-zero root. Odd levels duplicate the last
/// node (Bitcoin-style), which is fine here because transaction count is
/// committed in the block and validated separately.
pub fn tx_merkle_root(transactions: &[Vec<u8>]) -> Hash {
    if transactions.is_empty() {
        return Hash::default();
    }
    let mut level: Vec<[u8; 32]> = transactions
        .iter()
        .map(|tx| *blake3::hash(tx).as_bytes())
        .collect();

    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        level = level
            .chunks_exact(2)
            .map(|pair| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&pair[0]);
                hasher.update(&pair[1]);
                *hasher.finalize().as_bytes()
            })
            .collect();
    }
    Hash::new_from_array(level[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_header() -> BlockHeader {
        BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::default(),
            height: 0,
            timestamp: 1_750_000_000,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target: [0xff; 32],
            nonce: 0,
            miner: Pubkey::default(),
        }
    }

    #[test]
    fn id_changes_with_nonce() {
        let mut h = dummy_header();
        let id0 = h.id();
        h.nonce = 1;
        assert_ne!(id0, h.id());
    }

    #[test]
    fn genesis_detection() {
        assert!(dummy_header().is_genesis());
    }

    #[test]
    fn empty_merkle_root_is_zero() {
        assert_eq!(tx_merkle_root(&[]), Hash::default());
    }

    #[test]
    fn merkle_root_is_deterministic_and_order_sensitive() {
        let a = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];
        let b = vec![vec![4u8, 5, 6], vec![1u8, 2, 3]];
        assert_eq!(tx_merkle_root(&a), tx_merkle_root(&a));
        assert_ne!(tx_merkle_root(&a), tx_merkle_root(&b));
    }
}
