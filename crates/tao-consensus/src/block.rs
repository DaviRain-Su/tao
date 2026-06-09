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
        bincode::serialize(self).expect("block header serialization is infallible")
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
        Self {
            header,
            transactions,
        }
    }

    pub fn id(&self) -> BlockId {
        self.header.id()
    }
}

/// blockDAG header — the M8 multi-parent generalization of [`BlockHeader`].
///
/// It shares the linear header's identity/PoW hashing scheme (BLAKE3 over the
/// bincode-encoded header) and field types, replacing the single `prev_hash`
/// with a set of GHOSTDAG `parents` and dropping the linear `height` (a DAG
/// block's position is its GHOSTDAG blue score, not a scalar height).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagBlockHeader {
    /// Header format version.
    pub version: u32,
    /// Parent block ids (the GHOSTDAG parents). Empty only for genesis.
    pub parents: Vec<BlockId>,
    /// Block timestamp (unix seconds).
    pub timestamp: i64,
    /// Merkle root over the block's transactions.
    pub tx_merkle_root: Hash,
    /// Commitment to post-execution account state. Reserved: the DAG keeps state
    /// as a derived cache (see `tao-dagvm`), so it is left zero for now.
    #[serde(default)]
    pub state_root: Hash,
    /// PoW target threshold (big-endian). `pow_hash <= target` wins.
    pub target: Target,
    /// NiPoPoW interlink: `interlink[k]` is the most recent selected ancestor of
    /// PoW level ≥ k. Committed to by PoW and validated on accept, so it can't be
    /// forged; it lets a pruned node retain a succinct proof of accumulated work.
    /// Empty for genesis.
    #[serde(default)]
    pub interlink: Vec<BlockId>,
    /// PoW solution nonce.
    pub nonce: u64,
    /// Address that receives this block's coinbase reward.
    pub miner: Pubkey,
}

impl DagBlockHeader {
    /// Serialize the header deterministically (bincode) for hashing and storage.
    pub fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).expect("block header serialization is infallible")
    }

    /// The block id = BLAKE3 of the serialized header (also the PoW hash for the
    /// Blake3 PoW). Same scheme as [`BlockHeader::id`].
    pub fn id(&self) -> BlockId {
        *blake3::hash(&self.serialize()).as_bytes()
    }
}

/// A full blockDAG block: a multi-parent header plus serialized transactions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DagBlock {
    pub header: DagBlockHeader,
    pub transactions: Vec<Vec<u8>>,
}

impl DagBlock {
    pub fn new(header: DagBlockHeader, transactions: Vec<Vec<u8>>) -> Self {
        Self {
            header,
            transactions,
        }
    }

    pub fn id(&self) -> BlockId {
        self.header.id()
    }

    /// Decode a DAG block with legacy header compatibility.
    ///
    /// Supports the current header format and earlier serialized forms that may
    /// still exist in long-lived nodes.
    pub fn from_bytes(bytes: &[u8]) -> bincode::Result<Self> {
        if let Ok(block) = bincode::deserialize::<Self>(bytes) {
            return Ok(block);
        }
        if let Ok(block) = bincode::deserialize::<LegacyDagBlockV2>(bytes) {
            return Ok(block.into());
        }
        if let Ok(block) = bincode::deserialize::<LegacyDagBlockV1>(bytes) {
            return Ok(block.into());
        }
        bincode::deserialize::<Self>(bytes)
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

// Legacy M8 block formats used before `interlink`/migration compatibility.
#[derive(Serialize, Deserialize)]
struct LegacyDagBlockV1 {
    /// Header format version.
    pub version: u32,
    /// Parent block ids (the GHOSTDAG parents). Empty only for genesis.
    pub parents: Vec<BlockId>,
    /// Block timestamp (unix seconds).
    pub timestamp: i64,
    /// Merkle root over the block's transactions.
    pub tx_merkle_root: Hash,
    /// PoW target threshold (big-endian). `pow_hash <= target` wins.
    pub target: Target,
    /// PoW solution nonce.
    pub nonce: u64,
    /// Address that receives this block's coinbase reward.
    pub miner: Pubkey,
    pub transactions: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
struct LegacyDagBlockV2 {
    /// Header format version.
    pub version: u32,
    /// Parent block ids (the GHOSTDAG parents). Empty only for genesis.
    pub parents: Vec<BlockId>,
    /// Block timestamp (unix seconds).
    pub timestamp: i64,
    /// Merkle root over the block's transactions.
    pub tx_merkle_root: Hash,
    /// Commitment to post-execution account state.
    pub state_root: Hash,
    /// PoW target threshold (big-endian). `pow_hash <= target` wins.
    pub target: Target,
    /// PoW solution nonce.
    pub nonce: u64,
    /// Address that receives this block's coinbase reward.
    pub miner: Pubkey,
    pub transactions: Vec<Vec<u8>>,
}

impl From<LegacyDagBlockV1> for DagBlock {
    fn from(value: LegacyDagBlockV1) -> Self {
        Self {
            header: DagBlockHeader {
                version: value.version,
                parents: value.parents,
                timestamp: value.timestamp,
                tx_merkle_root: value.tx_merkle_root,
                state_root: Hash::default(),
                target: value.target,
                interlink: Vec::new(),
                nonce: value.nonce,
                miner: value.miner,
            },
            transactions: value.transactions,
        }
    }
}

impl From<LegacyDagBlockV2> for DagBlock {
    fn from(value: LegacyDagBlockV2) -> Self {
        Self {
            header: DagBlockHeader {
                version: value.version,
                parents: value.parents,
                timestamp: value.timestamp,
                tx_merkle_root: value.tx_merkle_root,
                state_root: value.state_root,
                target: value.target,
                interlink: Vec::new(),
                nonce: value.nonce,
                miner: value.miner,
            },
            transactions: value.transactions,
        }
    }
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

    #[test]
    fn dag_block_from_bytes_accepts_current_format() {
        let block = DagBlock {
            header: DagBlockHeader {
                version: HEADER_VERSION,
                parents: vec![[2u8; 32]],
                timestamp: 1_750_000_000,
                tx_merkle_root: Hash::default(),
                state_root: Hash::default(),
                target: [0xff; 32],
                interlink: vec![[3u8; 32]],
                nonce: 42,
                miner: Pubkey::default(),
            },
            transactions: vec![vec![1, 2, 3]],
        };
        let bytes = bincode::serialize(&block).unwrap();
        let decoded = DagBlock::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, block);
    }

    #[test]
    fn dag_block_from_bytes_accepts_pre_interlink_format() {
        let legacy = LegacyDagBlockV2 {
            version: HEADER_VERSION,
            parents: vec![[4u8; 32]],
            timestamp: 1_750_000_001,
            tx_merkle_root: Hash::default(),
            state_root: Hash::new_from_array([9u8; 32]),
            target: [0xee; 32],
            nonce: 7,
            miner: Pubkey::default(),
            transactions: vec![vec![9]],
        };
        let bytes = bincode::serialize(&legacy).unwrap();
        let decoded = DagBlock::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.header.state_root, legacy.state_root);
        assert!(decoded.header.interlink.is_empty());
    }

    #[test]
    fn dag_block_from_bytes_accepts_legacy_format() {
        let legacy = LegacyDagBlockV1 {
            version: HEADER_VERSION,
            parents: vec![[8u8; 32]],
            timestamp: 1_750_000_002,
            tx_merkle_root: Hash::default(),
            target: [0xdd; 32],
            nonce: 3,
            miner: Pubkey::default(),
            transactions: vec![vec![2]],
        };
        let bytes = bincode::serialize(&legacy).unwrap();
        let decoded = DagBlock::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.header.state_root, Hash::default());
        assert!(decoded.header.interlink.is_empty());
    }
}
