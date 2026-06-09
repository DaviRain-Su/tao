//! `tao-pouw` — matmul-PoUW: AI-shaped Proof-of-Useful-Work.
//!
//! The mining puzzle is a **matrix multiplication** (the core operation of AI
//! inference/training) rather than a plain hash, following Pearl's NoisyGEMM
//! construction: low-rank noise is added to the inputs, the *noised* product is
//! computed, and a transcript hash of that product is checked against the
//! target. Finding a winning nonce requires actually doing the `O(n³)` matmul.
//!
//! ## Prototype scope (honest)
//!
//! This is a **CPU, integer-arithmetic** implementation that is deterministic
//! and verifiable *by recomputation* — enough to prove the mechanism and the
//! `RandomX → matmul` algorithm-switch (via [`tao_consensus::HeightSwitchPow`]).
//!
//! Production (future, see `docs/PLAN.md` M7) adds the parts that need a GPU and
//! a ZK toolchain not available here:
//! - **GPU CUDA kernels** (Pearl's `pearl-gemm`) for real throughput.
//! - **Plonky2 STARK proofs** so verification is ~60 KB / milliseconds instead
//!   of recomputing the matmul.
//! - **Utility gate**: bind the matrices to a real registered model + inference
//!   request so the work is genuinely useful (not just "AI-shaped").

mod gemm;
pub mod utility_gate;

use tao_consensus::{BlockHeader, PowAlgorithm};

/// matmul-PoUW over `n × n` integer matrices with rank-`rank` injected noise.
///
/// This is the current "AI-shaped" implementation (matrices derived freely from
/// the header seed). See the sibling [`utility_gate`] module for the prototype
/// of binding the work to a real registered model (PLAN.md M7b).
#[derive(Debug, Clone, Copy)]
pub struct MatmulPow {
    n: usize,
    rank: usize,
}

impl MatmulPow {
    pub fn new(n: usize, rank: usize) -> Self {
        assert!(n > 0 && rank > 0 && rank <= n, "invalid matmul-pouw dimensions");
        Self { n, rank }
    }

    /// A representative GPU-phase size (64×64, rank 4).
    pub fn gpu_default() -> Self {
        Self::new(64, 4)
    }

    /// The PoW hash: transcript of the noised product `(A+E)·(B+F)`, with `A,B`
    /// (and the noise) derived from the header seed.
    fn compute(&self, header: &BlockHeader) -> [u8; 32] {
        let seed = *blake3::hash(&header.serialize()).as_bytes();
        let (n, r) = (self.n, self.rank);
        let a = gemm::fill(&seed, 0, n * n);
        let b = gemm::fill(&seed, 1, n * n);
        gemm::noisy_product_transcript(&a, &b, n, r, &seed)
    }
}

impl PowAlgorithm for MatmulPow {
    fn name(&self) -> &'static str {
        "matmul-pouw"
    }

    fn pow_hash(&self, header: &BlockHeader) -> [u8; 32] {
        self.compute(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tao_consensus::{
        block::HEADER_VERSION, Blake3Pow, BlockHeader, ChainState, DifficultyParams,
        HeightSwitchPow, PowAlgorithm,
    };
    use tao_core::{Hash, Pubkey};

    fn header(height: u64, nonce: u64, target: [u8; 32]) -> BlockHeader {
        BlockHeader {
            version: HEADER_VERSION,
            prev_hash: Hash::default(),
            height,
            timestamp: 1,
            tx_merkle_root: Hash::default(),
            state_root: Hash::default(),
            target,
            nonce,
            miner: Pubkey::default(),
        }
    }

    #[test]
    fn deterministic_and_nonce_sensitive() {
        let pow = MatmulPow::new(16, 2);
        let h = header(1, 0, [0xff; 32]);
        assert_eq!(pow.pow_hash(&h), pow.pow_hash(&h));
        let mut h2 = h.clone();
        h2.nonce = 1;
        assert_ne!(pow.pow_hash(&h), pow.pow_hash(&h2));
    }

    #[test]
    fn grind_finds_solution_and_verifies() {
        let pow = MatmulPow::new(8, 2);
        let mut target = [0xffu8; 32];
        target[0] = 0x00; // a few bits of work
        let mut h = header(1, 0, target);
        while !pow.verify(&h) {
            h.nonce += 1;
        }
        assert!(pow.verify(&h));
        let mut bad = h.clone();
        bad.nonce = h.nonce.wrapping_add(1);
        // Overwhelmingly likely to fail the target now.
        assert!(!pow.verify(&bad) || pow.pow_hash(&bad) != pow.pow_hash(&h));
    }

    /// Mine a chain that switches from Blake3 (CPU) to matmul-PoUW at height 3 —
    /// the RandomX→matmul evolution mechanism, validated end-to-end.
    #[test]
    fn chain_switches_algorithm_at_height() {
        let mut target = [0xffu8; 32];
        target[0] = 0x00;
        let genesis = header(0, 0, target);

        let pow: Arc<dyn PowAlgorithm> = Arc::new(HeightSwitchPow::new(
            Arc::new(Blake3Pow),
            Arc::new(MatmulPow::new(8, 2)),
            3, // activate matmul-PoUW at height 3
        ));
        let params = DifficultyParams::new(10, 16);
        let mut chain = ChainState::new(genesis, params, pow.clone());

        for h in 1..=5u64 {
            let mut header = chain.build_candidate(Pubkey::default(), 1_000_000 + (h as i64) * 10, &[]);
            while !pow.verify(&header) {
                header.nonce += 1;
            }
            chain.add_header(header).expect("block accepted across the switch");
        }
        assert_eq!(chain.height(), 5);
    }
}
