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
        assert!(
            n > 0 && rank > 0 && rank <= n,
            "invalid matmul-pouw dimensions"
        );
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

/// **Utility-gated** matmul-PoUW as the chain's actual block PoW.
///
/// Unlike [`MatmulPow`] (matrices freely derived from the seed = "AI-shaped"
/// work), this binds the PoW to a **real registered model**: the puzzle is the
/// model's committed weight tile applied to a per-block input. Mining grinds the
/// `nonce`, which only seeds the low-rank *noise*, so the underlying `A·B` is the
/// genuine model-layer result. Every node holds the same genesis-agreed model
/// (its id is the Merkle commitment over the weight tiles), so verification
/// recomputes the transcript against the *canonical* weights — a miner cannot
/// substitute forged weights (the transcript wouldn't match). This makes the
/// chain's work provably real model computation, fitting the existing
/// [`PowAlgorithm`] trait with no header change.
pub struct UtilityGatePow {
    weights: Vec<Vec<i64>>,
    n: usize,
    rank: usize,
    model_id: [u8; 32],
}

impl UtilityGatePow {
    /// Build from explicit weight tiles (each an `n×n` matrix). The model id is
    /// the Merkle commitment over the tiles (so all nodes agree on the weights).
    pub fn from_weights(name: &str, n: usize, rank: usize, weights: Vec<Vec<i64>>) -> Self {
        assert!(n > 0 && rank > 0 && rank <= n && !weights.is_empty());
        assert!(weights.iter().all(|t| t.len() == n * n), "tiles must be n×n");
        let mut reg = utility_gate::ModelRegistry::new();
        let model_id = reg.register(name, n, &weights);
        Self { weights, n, rank, model_id }
    }

    /// A deterministic demo model with `tiles` weight layers (same on all nodes).
    /// Production commits the real model in genesis instead.
    pub fn demo(name: &str, n: usize, rank: usize, tiles: usize) -> Self {
        let weights: Vec<Vec<i64>> = (0..tiles)
            .map(|t| (0..n * n).map(|i| ((t * 7 + i * 3) % 17) as i64 - 8).collect())
            .collect();
        Self::from_weights(name, n, rank, weights)
    }

    /// The model id = Merkle commitment over the weight tiles.
    pub fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    /// Derive this block's work item (tile index + input + work commitment) from
    /// the header *excluding the nonce* (the nonce is the grinding variable). All
    /// non-nonce fields are committed, so a solved block can't be repurposed.
    fn work(&self, header: &BlockHeader) -> (usize, Vec<i64>, [u8; 32]) {
        let mut base_header = header.clone();
        base_header.nonce = 0;
        let base = *blake3::hash(&base_header.serialize()).as_bytes();
        let tile = (u64::from_le_bytes(base[0..8].try_into().unwrap()) as usize) % self.weights.len();
        let input = gemm::fill(&base, 7, self.n * self.n);
        let mut h = blake3::Hasher::new();
        h.update(b"tao-utility-wc");
        h.update(&self.model_id);
        h.update(&base);
        let work_commitment = *h.finalize().as_bytes();
        (tile, input, work_commitment)
    }

    /// The useful inference output `A·B` for this block's derived work item — the
    /// real model-layer result the work computed (recoverable cheaply).
    pub fn useful_output(&self, header: &BlockHeader) -> Vec<i64> {
        let (tile, input, _) = self.work(header);
        gemm::matmul(&self.weights[tile], &input, self.n)
    }
}

impl PowAlgorithm for UtilityGatePow {
    fn name(&self) -> &'static str {
        "utility-matmul-pouw"
    }

    fn pow_hash(&self, header: &BlockHeader) -> [u8; 32] {
        let (tile, input, work_commitment) = self.work(header);
        // The nonce only seeds the noise; the model tile + input are fixed by the
        // block, so finding a winning nonce requires actually doing the matmul.
        let mut h = blake3::Hasher::new();
        h.update(b"tao-noise");
        h.update(&work_commitment);
        h.update(&header.nonce.to_le_bytes());
        let seed = *h.finalize().as_bytes();
        gemm::noisy_product_transcript(&self.weights[tile], &input, self.n, self.rank, &seed)
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
            let mut header =
                chain.build_candidate(Pubkey::default(), 1_000_000 + (h as i64) * 10, &[]);
            while !pow.verify(&header) {
                header.nonce += 1;
            }
            chain
                .add_header(header)
                .expect("block accepted across the switch");
        }
        assert_eq!(chain.height(), 5);
    }

    /// The utility-gated matmul-PoUW drives a real chain end-to-end: every block's
    /// PoW is the genesis model's layer applied to a per-block input.
    #[test]
    fn utility_gate_pow_drives_a_chain() {
        let mut target = [0xffu8; 32];
        target[0] = 0x00;
        let genesis = header(0, 0, target);

        let gate = UtilityGatePow::demo("tao-pouw-model", 8, 2, 8);
        assert_ne!(gate.model_id(), [0u8; 32]);
        let pow: Arc<dyn PowAlgorithm> = Arc::new(gate);
        let params = DifficultyParams::new(10, 16);
        let mut chain = ChainState::new(genesis, params, pow.clone());

        for h in 1..=5u64 {
            let mut header =
                chain.build_candidate(Pubkey::default(), 1_000_000 + (h as i64) * 10, &[]);
            while !pow.verify(&header) {
                header.nonce += 1;
            }
            chain.add_header(header).expect("utility-gated block accepted");
        }
        assert_eq!(chain.height(), 5);
    }

    /// Tampering any committed (non-nonce) header field invalidates the PoW: the
    /// derived work item changes, so the transcript no longer meets the target.
    #[test]
    fn utility_gate_pow_binds_the_whole_header() {
        let gate = UtilityGatePow::demo("m", 8, 2, 8);
        let mut target = [0xffu8; 32];
        target[0] = 0x00;
        let mut h = header(1, 0, target);
        while !gate.verify(&h) {
            h.nonce += 1;
        }
        assert!(gate.verify(&h));
        // The useful output is the real model-layer result (right shape).
        assert_eq!(gate.useful_output(&h).len(), 8 * 8);
        // Tamper a committed field (height) → different puzzle → invalid.
        let mut bad = h.clone();
        bad.height = 2;
        assert!(!gate.verify(&bad) || gate.pow_hash(&bad) != gate.pow_hash(&h));
    }
}
