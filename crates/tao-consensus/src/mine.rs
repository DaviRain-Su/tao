//! Single-threaded PoW mining helpers (grind a nonce until the target is met).
//!
//! The node drives timing, persistence, and logging; this module just provides
//! the inner grind and candidate-template construction.

use crate::block::BlockHeader;
use crate::pow::PowAlgorithm;

/// Outcome of a grind attempt.
pub enum GrindResult {
    /// Found a valid nonce after this many hash attempts.
    Found { hashes: u64 },
    /// Exhausted `max_iters` without a solution (caller refreshes timestamp).
    Exhausted { hashes: u64 },
}

/// Grind `header.nonce` until [`PowAlgorithm::verify`] passes or `max_iters`
/// attempts are exhausted. On success `header` holds the winning nonce.
pub fn grind(header: &mut BlockHeader, pow: &dyn PowAlgorithm, max_iters: u64) -> GrindResult {
    for i in 0..max_iters {
        if pow.verify(header) {
            return GrindResult::Found { hashes: i + 1 };
        }
        header.nonce = header.nonce.wrapping_add(1);
    }
    GrindResult::Exhausted { hashes: max_iters }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::HEADER_VERSION;
    use crate::pow::Blake3Pow;
    use tao_core::{Hash, Pubkey};

    #[test]
    fn grind_finds_nonce_for_easy_target() {
        let mut target = [0xffu8; 32];
        target[0] = 0x0f; // a few bits of work
        let mut header = BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::default(),
            height: 1,
            timestamp: 1,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target,
            nonce: 0,
            miner: Pubkey::default(),
        };
        assert!(matches!(
            grind(&mut header, &Blake3Pow, 1_000_000),
            GrindResult::Found { .. }
        ));
        assert!(Blake3Pow.verify(&header));
    }
}
