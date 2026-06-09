//! In-memory chain state and fork choice.
//!
//! `ChainState` indexes headers by id, tracks cumulative work, and selects the
//! best tip by the **most-cumulative-work** rule (Bitcoin's fork choice,
//! generalized from "longest" to "heaviest"). Persistence (RocksDB) is layered
//! on top in `tao-database`; this type is the pure consensus core.

use std::collections::HashMap;
use std::sync::Arc;

use primitive_types::U256;
use tao_core::{Hash, Pubkey};

use crate::block::{tx_merkle_root, BlockHeader, BlockId, HEADER_VERSION};
use crate::difficulty::{next_target, DifficultyParams};
use crate::pow::PowAlgorithm;
use crate::target::{work_for_target, Target};

/// Result of accepting a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockStatus {
    /// Extended the current best tip.
    ExtendedTip,
    /// Replaced the best tip via a heavier branch (reorg).
    Reorg { old_tip: BlockId, new_tip: BlockId },
    /// Valid but not the best tip (stored as a side branch).
    SideChain,
}

/// Why a block was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("duplicate block")]
    Duplicate,
    #[error("unknown parent (orphan)")]
    UnknownParent,
    #[error("bad height: expected {expected}, got {got}")]
    BadHeight { expected: u64, got: u64 },
    #[error("non-increasing timestamp")]
    BadTimestamp,
    #[error("wrong difficulty target")]
    BadTarget,
    #[error("invalid proof-of-work")]
    BadPow,
}

struct Entry {
    header: BlockHeader,
    cumulative_work: U256,
}

/// The consensus chain state.
pub struct ChainState {
    blocks: HashMap<BlockId, Entry>,
    tip: BlockId,
    genesis: BlockId,
    params: DifficultyParams,
    genesis_target: Target,
    pow: Arc<dyn PowAlgorithm>,
}

impl ChainState {
    /// Create a new chain initialized at `genesis`. The genesis PoW is not
    /// checked (it is a fixed protocol constant).
    pub fn new(genesis: BlockHeader, params: DifficultyParams, pow: Arc<dyn PowAlgorithm>) -> Self {
        assert!(
            genesis.is_genesis(),
            "genesis header must be at height 0 with zero prev_hash"
        );
        let id = genesis.id();
        let genesis_target = genesis.target;
        let work = work_for_target(&genesis.target);
        let mut blocks = HashMap::new();
        blocks.insert(
            id,
            Entry {
                header: genesis,
                cumulative_work: work,
            },
        );
        Self {
            blocks,
            tip: id,
            genesis: id,
            params,
            genesis_target,
            pow,
        }
    }

    pub fn genesis_id(&self) -> BlockId {
        self.genesis
    }

    pub fn tip_id(&self) -> BlockId {
        self.tip
    }

    pub fn tip_header(&self) -> &BlockHeader {
        &self.blocks[&self.tip].header
    }

    /// Height of the best tip.
    pub fn height(&self) -> u64 {
        self.tip_header().height
    }

    /// Total accumulated work at the best tip.
    pub fn tip_work(&self) -> U256 {
        self.blocks[&self.tip].cumulative_work
    }

    pub fn contains(&self, id: &BlockId) -> bool {
        self.blocks.contains_key(id)
    }

    pub fn header(&self, id: &BlockId) -> Option<&BlockHeader> {
        self.blocks.get(id).map(|e| &e.header)
    }

    /// Build an unmined candidate header extending the best tip. The caller
    /// grinds the nonce (see [`crate::mine::grind`]) before submitting it back
    /// via [`ChainState::add_header`].
    pub fn build_candidate(
        &self,
        miner: Pubkey,
        timestamp: i64,
        transactions: &[Vec<u8>],
    ) -> BlockHeader {
        let parent = self.tip_header();
        BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::new_from_array(self.tip),
            height: parent.height + 1,
            timestamp,
            tx_merkle_root: tx_merkle_root(transactions),
            state_root: Hash::default(),
            target: self.next_target(),
            nonce: 0,
            miner,
        }
    }

    /// The target the next block on top of the best tip must use.
    pub fn next_target(&self) -> Target {
        let recent = self.recent_headers(self.tip, self.params.window as usize + 1);
        next_target(&recent, &self.params, &self.genesis_target)
    }

    /// Walk back up to `count` headers from `from` (inclusive), returned in
    /// ascending-height order.
    pub fn recent_headers(&self, from: BlockId, count: usize) -> Vec<BlockHeader> {
        let mut out = Vec::with_capacity(count);
        let mut cur = from;
        for _ in 0..count {
            let Some(entry) = self.blocks.get(&cur) else {
                break;
            };
            out.push(entry.header.clone());
            if entry.header.is_genesis() {
                break;
            }
            cur = hash_to_id(&entry.header.prev_hash);
        }
        out.reverse();
        out
    }

    /// Validate and add a header to the chain, updating the best tip.
    pub fn add_header(&mut self, header: BlockHeader) -> Result<BlockStatus, ChainError> {
        let id = header.id();
        if self.blocks.contains_key(&id) {
            return Err(ChainError::Duplicate);
        }
        let parent_id = hash_to_id(&header.prev_hash);
        let parent = self
            .blocks
            .get(&parent_id)
            .ok_or(ChainError::UnknownParent)?;

        // Structural checks.
        if header.height != parent.header.height + 1 {
            return Err(ChainError::BadHeight {
                expected: parent.header.height + 1,
                got: header.height,
            });
        }
        if header.timestamp < parent.header.timestamp {
            return Err(ChainError::BadTimestamp);
        }

        // Difficulty must match what the protocol computes for this parent.
        let recent = self.recent_headers(parent_id, self.params.window as usize + 1);
        let expected = next_target(&recent, &self.params, &self.genesis_target);
        if header.target != expected {
            return Err(ChainError::BadTarget);
        }

        // Proof-of-work.
        if !self.pow.verify(&header) {
            return Err(ChainError::BadPow);
        }

        let cumulative_work = parent.cumulative_work + work_for_target(&header.target);
        self.blocks.insert(
            id,
            Entry {
                header,
                cumulative_work,
            },
        );

        // Fork choice: heaviest cumulative work wins; ties keep the incumbent.
        if cumulative_work > self.blocks[&self.tip].cumulative_work {
            let old_tip = self.tip;
            self.tip = id;
            if hash_to_id(&self.blocks[&id].header.prev_hash) == old_tip {
                Ok(BlockStatus::ExtendedTip)
            } else {
                Ok(BlockStatus::Reorg {
                    old_tip,
                    new_tip: id,
                })
            }
        } else {
            Ok(BlockStatus::SideChain)
        }
    }
}

/// Convert a `Hash` (parent pointer) into a `BlockId`.
fn hash_to_id(hash: &Hash) -> BlockId {
    hash.to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::HEADER_VERSION;
    use crate::pow::Blake3Pow;
    use tao_core::{Hash, Pubkey};

    fn params() -> DifficultyParams {
        DifficultyParams::new(10, 16)
    }

    fn genesis() -> BlockHeader {
        let mut target = [0xffu8; 32];
        target[0] = 0x00; // easy
        BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::default(),
            height: 0,
            timestamp: 1_000_000,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target,
            nonce: 0,
            miner: Pubkey::default(),
        }
    }

    /// Mine a valid child header on top of `parent` with the given timestamp.
    fn mine_child(chain: &ChainState, parent: &BlockHeader, timestamp: i64) -> BlockHeader {
        let pow = Blake3Pow;
        let target = {
            let recent = chain.recent_headers(parent.id(), chain.params.window as usize + 1);
            next_target(&recent, &chain.params, &chain.genesis_target)
        };
        let mut header = BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::new_from_array(parent.id()),
            height: parent.height + 1,
            timestamp,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target,
            nonce: 0,
            miner: Pubkey::default(),
        };
        while !pow.verify(&header) {
            header.nonce += 1;
        }
        header
    }

    #[test]
    fn extends_tip() {
        let mut chain = ChainState::new(genesis(), params(), Arc::new(Blake3Pow));
        let parent = chain.tip_header().clone();
        let child = mine_child(&chain, &parent, 1_000_010);
        assert_eq!(chain.add_header(child).unwrap(), BlockStatus::ExtendedTip);
        assert_eq!(chain.height(), 1);
    }

    #[test]
    fn rejects_duplicate_and_orphan() {
        let mut chain = ChainState::new(genesis(), params(), Arc::new(Blake3Pow));
        let parent = chain.tip_header().clone();
        let child = mine_child(&chain, &parent, 1_000_010);
        chain.add_header(child.clone()).unwrap();
        assert_eq!(chain.add_header(child).unwrap_err(), ChainError::Duplicate);

        // Orphan: parent pointer to an unknown block.
        let mut orphan = mine_child(&chain, chain.tip_header(), 1_000_020);
        orphan.prev_hash = Hash::new_from_array([9u8; 32]);
        // recompute pow not needed; parent lookup fails first
        assert_eq!(
            chain.add_header(orphan).unwrap_err(),
            ChainError::UnknownParent
        );
    }

    #[test]
    fn heavier_branch_triggers_reorg() {
        let mut chain = ChainState::new(genesis(), params(), Arc::new(Blake3Pow));
        let g = chain.tip_header().clone();

        // Branch A: single block on genesis.
        let a1 = mine_child(&chain, &g, 1_000_010);
        chain.add_header(a1.clone()).unwrap();
        assert_eq!(chain.tip_id(), a1.id());

        // Branch B: two blocks on genesis → heavier → should win.
        let b1 = mine_child(&chain, &g, 1_000_011);
        assert_eq!(
            chain.add_header(b1.clone()).unwrap(),
            BlockStatus::SideChain
        );
        let b2 = mine_child(&chain, &b1, 1_000_021);
        let status = chain.add_header(b2.clone()).unwrap();
        assert!(matches!(status, BlockStatus::Reorg { .. }));
        assert_eq!(chain.tip_id(), b2.id());
        assert_eq!(chain.height(), 2);
    }
}
