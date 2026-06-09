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

use std::sync::Arc;

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

/// Switch PoW algorithms at a predetermined block height (a hard fork).
///
/// This is the mining-hardware evolution mechanism: e.g. RandomX (CPU fair
/// launch) below `activation_height`, then matmul-PoUW (GPU, AI-shaped) at and
/// above it. At the switch a chain should also reset difficulty and add a
/// checkpoint to survive the low-difficulty window (see the plan).
pub struct HeightSwitchPow {
    before: Arc<dyn PowAlgorithm>,
    after: Arc<dyn PowAlgorithm>,
    activation_height: u64,
}

impl HeightSwitchPow {
    pub fn new(
        before: Arc<dyn PowAlgorithm>,
        after: Arc<dyn PowAlgorithm>,
        activation_height: u64,
    ) -> Self {
        Self { before, after, activation_height }
    }

    fn active(&self, height: u64) -> &Arc<dyn PowAlgorithm> {
        if height >= self.activation_height {
            &self.after
        } else {
            &self.before
        }
    }
}

impl PowAlgorithm for HeightSwitchPow {
    fn name(&self) -> &'static str {
        "height-switch"
    }

    fn pow_hash(&self, header: &BlockHeader) -> [u8; 32] {
        self.active(header.height).pow_hash(header)
    }

    fn verify(&self, header: &BlockHeader) -> bool {
        self.active(header.height).verify(header)
    }
}
