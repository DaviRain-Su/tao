//! `Blake3Pow` — a simple BLAKE3-based PoW for bootstrapping and tests.
//!
//! Not ASIC-resistant and not the production launch algorithm (that is RandomX,
//! a follow-up within M1). It exists so the consensus engine, miner, and tests
//! can run with zero native dependencies.

use crate::block::BlockHeader;

use super::PowAlgorithm;

/// BLAKE3 over the serialized header.
#[derive(Debug, Default, Clone, Copy)]
pub struct Blake3Pow;

impl PowAlgorithm for Blake3Pow {
    fn name(&self) -> &'static str {
        "blake3"
    }

    fn pow_hash(&self, header: &BlockHeader) -> [u8; 32] {
        *blake3::hash(&header.serialize()).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::HEADER_VERSION;
    use tao_core::{Hash, Pubkey};

    fn header() -> BlockHeader {
        BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::default(),
            height: 0,
            timestamp: 0,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target: [0xff; 32], // trivially easy: every hash meets it
            nonce: 0,
            miner: Pubkey::default(),
        }
    }

    #[test]
    fn verifies_against_easy_target() {
        let pow = Blake3Pow;
        assert!(pow.verify(&header()));
    }

    #[test]
    fn rejects_impossible_target() {
        let pow = Blake3Pow;
        let mut h = header();
        h.target = [0u8; 32]; // only hash == 0 would pass
        assert!(!pow.verify(&h));
    }
}
