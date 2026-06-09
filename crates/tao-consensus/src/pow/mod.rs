//! Proof-of-Work algorithm abstraction.
//!
//! The chain commits to a *target* in each header; an algorithm turns a header
//! into a 256-bit PoW hash, and the block is valid when that hash meets the
//! target. Keeping this behind a trait lets the chain switch algorithms at a
//! predetermined activation height:
//!
//! - **Phase 1 (CPU fair launch):** RandomX (`RandomXPow`, follow-up).
//! - **Phase 2 (GPU, AI-shaped):** matmul-PoUW with STARK verification (M7).
//!
//! [`Blake3Pow`] is a fast, dependency-free algorithm used to bring the
//! consensus engine up and to drive tests and local devnets.

mod blake3pow;

pub use blake3pow::Blake3Pow;

use crate::block::BlockHeader;
use crate::target::meets_target;

/// A pluggable PoW algorithm.
pub trait PowAlgorithm: Send + Sync {
    /// Human-readable algorithm name (for logs and headers).
    fn name(&self) -> &'static str;

    /// Compute the 256-bit PoW hash of a header (must depend on `header.nonce`).
    fn pow_hash(&self, header: &BlockHeader) -> [u8; 32];

    /// Verify the header's PoW hash meets its committed target.
    fn verify(&self, header: &BlockHeader) -> bool {
        meets_target(&self.pow_hash(header), &header.target)
    }
}
