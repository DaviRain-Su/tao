//! Utility gate — bind matmul-PoUW to a **real registered model + input**.
//!
//! ## The problem this solves
//!
//! Pearl's PoW verifies that a matrix multiplication was performed *correctly*,
//! but not that it corresponds to anything real — a miner can feed random
//! matrices and still win. Independent analysis found the live network does
//! ≈zero useful AI for exactly this reason ("AI-shaped work, not useful work").
//!
//! ## The design
//!
//! Make the puzzle's matrices **not free**:
//! - A **model** is registered on-chain with a Merkle commitment over its weight
//!   tiles (so everyone agrees on the exact weights).
//! - A **work item** = (model, tile index, a real input matrix). It comes from a
//!   user who wants that inference computed (and pays for it).
//! - A valid solution must use `A` = the model's committed weight tile (proven
//!   by a Merkle proof against the model's weight root) and `B` = the requested
//!   input. The nonce only seeds the low-rank *noise* (for PoW grinding), so the
//!   underlying `A·B` — the **useful inference result** — is fixed by the task.
//!
//! A miner using random/forged weights fails the Merkle check, so the work is
//! provably a real model computation. The true output `A·B` is recovered
//! cheaply (here: directly) and returned to the requester — genuine PoUW.
//!
//! ## Prototype scope
//!
//! Pure CPU + integer, verified by recomputation. Production replaces the
//! recompute-to-verify with a Plonky2 STARK proof (so verifiers do ~60 KB /
//! ms work) and runs the GEMM on a GPU; the *binding protocol here is the part
//! Pearl is missing* and is the point of this module.

use std::collections::HashMap;

use tao_consensus::meets_target;

use crate::gemm;

// ----------------------------------------------------------------------------
// Merkle commitment over fixed-size tiles (power-of-two leaf count).
// ----------------------------------------------------------------------------

/// A Merkle inclusion proof (bottom-up sibling hashes + the leaf index).
#[derive(Debug, Clone)]
pub struct MerkleProof {
    pub index: usize,
    pub siblings: Vec<[u8; 32]>,
}

fn tile_leaf(tile: &[i64]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"tao-tile");
    for v in tile {
        h.update(&v.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

fn node_hash(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(l);
    h.update(r);
    *h.finalize().as_bytes()
}

fn next_pow2(n: usize) -> usize {
    let mut p = 1;
    while p < n {
        p <<= 1;
    }
    p
}

/// Pad `leaves` to a power of two with a fixed empty-leaf value.
fn padded_leaves(mut leaves: Vec<[u8; 32]>) -> Vec<[u8; 32]> {
    let target = next_pow2(leaves.len().max(1));
    let empty = *blake3::hash(b"tao-empty-tile").as_bytes();
    while leaves.len() < target {
        leaves.push(empty);
    }
    leaves
}

fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = level.chunks(2).map(|p| node_hash(&p[0], &p[1])).collect();
    }
    level[0]
}

fn merkle_proof(leaves: &[[u8; 32]], index: usize) -> MerkleProof {
    let mut level = leaves.to_vec();
    let mut idx = index;
    let mut siblings = Vec::new();
    while level.len() > 1 {
        siblings.push(level[idx ^ 1]);
        idx >>= 1;
        level = level.chunks(2).map(|p| node_hash(&p[0], &p[1])).collect();
    }
    MerkleProof { index, siblings }
}

fn merkle_verify(root: &[u8; 32], leaf: &[u8; 32], proof: &MerkleProof) -> bool {
    let mut h = *leaf;
    let mut idx = proof.index;
    for sib in &proof.siblings {
        h = if idx & 1 == 0 { node_hash(&h, sib) } else { node_hash(sib, &h) };
        idx >>= 1;
    }
    &h == root
}

// ----------------------------------------------------------------------------
// Model registry.
// ----------------------------------------------------------------------------

/// 32-byte model identifier (commits to the weights).
pub type ModelId = [u8; 32];

/// A registered model: its weight Merkle root + dimensions.
#[derive(Debug, Clone)]
pub struct RegisteredModel {
    pub name: String,
    pub n: usize,
    pub weight_root: [u8; 32],
    pub num_tiles: usize,
    padded_leaves: Vec<[u8; 32]>,
}

/// On-chain registry of models keyed by id.
#[derive(Default)]
pub struct ModelRegistry {
    models: HashMap<ModelId, RegisteredModel>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a model from its weight `tiles` (each an `n×n` matrix). Returns
    /// the model id, which commits to the weights (so it can't be swapped).
    pub fn register(&mut self, name: &str, n: usize, tiles: &[Vec<i64>]) -> ModelId {
        assert!(!tiles.is_empty() && tiles.iter().all(|t| t.len() == n * n));
        let leaves = padded_leaves(tiles.iter().map(|t| tile_leaf(t)).collect());
        let weight_root = merkle_root(&leaves);
        let mut h = blake3::Hasher::new();
        h.update(b"tao-model");
        h.update(name.as_bytes());
        h.update(&(n as u64).to_le_bytes());
        h.update(&weight_root);
        let id = *h.finalize().as_bytes();
        self.models.insert(
            id,
            RegisteredModel {
                name: name.to_string(),
                n,
                weight_root,
                num_tiles: tiles.len(),
                padded_leaves: leaves,
            },
        );
        id
    }

    pub fn get(&self, id: &ModelId) -> Option<&RegisteredModel> {
        self.models.get(id)
    }

    /// Produce the Merkle proof for tile `index` of a registered model (the
    /// model publisher / miner uses this to prove it used the real weights).
    pub fn tile_proof(&self, id: &ModelId, index: usize) -> Option<MerkleProof> {
        let m = self.models.get(id)?;
        Some(merkle_proof(&m.padded_leaves, index))
    }
}

// ----------------------------------------------------------------------------
// Work items and bound solutions.
// ----------------------------------------------------------------------------

/// A request to apply model `model_id`'s tile `tile_index` to `input` (`n×n`).
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub model_id: ModelId,
    pub tile_index: usize,
    pub input: Vec<i64>,
}

impl WorkItem {
    /// Commitment binding the model, tile, and input into one 32-byte value.
    pub fn commitment(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"tao-work");
        h.update(&self.model_id);
        h.update(&(self.tile_index as u64).to_le_bytes());
        h.update(&tile_leaf(&self.input));
        *h.finalize().as_bytes()
    }
}

/// A mined solution that binds the matmul to a real model tile + input.
#[derive(Debug, Clone)]
pub struct BoundSolution {
    pub work_commitment: [u8; 32],
    pub weight_tile: Vec<i64>,
    pub weight_proof: MerkleProof,
    pub input: Vec<i64>,
    pub nonce: u64,
}

/// Why a solution was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum GateError {
    UnknownModel,
    WorkMismatch,
    InputMismatch,
    ForgedWeights,
    PowNotMet,
}

/// Verifies (and mines) model-bound matmul-PoUW solutions.
pub struct UtilityGate {
    pub rank: usize,
}

impl UtilityGate {
    pub fn new(rank: usize) -> Self {
        Self { rank }
    }

    /// Noise seed for an attempt = H(work commitment ‖ nonce).
    fn noise_seed(work_commitment: &[u8; 32], nonce: u64) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"tao-noise");
        h.update(work_commitment);
        h.update(&nonce.to_le_bytes());
        *h.finalize().as_bytes()
    }

    /// Verify a solution against the registry, the work item, and the target.
    ///
    /// This is the gate Pearl lacks: it ties the PoW to a *real* model
    /// computation, so the work is genuinely useful and cannot be faked with
    /// random matrices.
    pub fn verify(
        &self,
        registry: &ModelRegistry,
        work: &WorkItem,
        target: &[u8; 32],
        sol: &BoundSolution,
    ) -> Result<(), GateError> {
        let model = registry.get(&work.model_id).ok_or(GateError::UnknownModel)?;

        // 1. The solution must be for this exact work item.
        if sol.work_commitment != work.commitment() {
            return Err(GateError::WorkMismatch);
        }
        if sol.input != work.input {
            return Err(GateError::InputMismatch);
        }
        // 2. The weights must be the model's real committed tile (anti-forgery).
        if sol.weight_proof.index != work.tile_index {
            return Err(GateError::ForgedWeights);
        }
        let leaf = tile_leaf(&sol.weight_tile);
        if !merkle_verify(&model.weight_root, &leaf, &sol.weight_proof) {
            return Err(GateError::ForgedWeights);
        }
        // 3. The noised product must meet the PoW target.
        let seed = Self::noise_seed(&sol.work_commitment, sol.nonce);
        let hash =
            gemm::noisy_product_transcript(&sol.weight_tile, &sol.input, model.n, self.rank, &seed);
        if !meets_target(&hash, target) {
            return Err(GateError::PowNotMet);
        }
        Ok(())
    }

    /// The useful output: the real layer result `A·B`. In production this is
    /// recovered cheaply from the noised product via the low-rank correction;
    /// here we compute it directly. This is what the requester actually wanted.
    pub fn useful_output(&self, sol: &BoundSolution, n: usize) -> Vec<i64> {
        gemm::matmul(&sol.weight_tile, &sol.input, n)
    }

    /// Mine a solution for `work` using the real `weight_tile` (+ its proof),
    /// grinding the nonce until the target is met.
    pub fn solve(
        &self,
        n: usize,
        work: &WorkItem,
        target: &[u8; 32],
        weight_tile: Vec<i64>,
        weight_proof: MerkleProof,
    ) -> BoundSolution {
        let work_commitment = work.commitment();
        let mut nonce = 0u64;
        loop {
            let seed = Self::noise_seed(&work_commitment, nonce);
            let hash =
                gemm::noisy_product_transcript(&weight_tile, &work.input, n, self.rank, &seed);
            if meets_target(&hash, target) {
                return BoundSolution {
                    work_commitment,
                    weight_tile,
                    weight_proof,
                    input: work.input.clone(),
                    nonce,
                };
            }
            nonce += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn easy_target() -> [u8; 32] {
        let mut t = [0xffu8; 32];
        t[0] = 0x00;
        t
    }

    /// Build a toy model: `tiles` weight blocks of size n×n, deterministic.
    fn toy_model(n: usize, tiles: usize) -> Vec<Vec<i64>> {
        (0..tiles)
            .map(|t| (0..n * n).map(|i| ((t * 7 + i * 3) % 17) as i64 - 8).collect())
            .collect()
    }

    #[test]
    fn accepts_real_model_computation_and_returns_useful_output() {
        let n = 8;
        let mut reg = ModelRegistry::new();
        let weights = toy_model(n, 5);
        let model_id = reg.register("toy-llm", n, &weights);

        let tile_index = 2;
        let input: Vec<i64> = (0..n * n).map(|i| (i % 5) as i64 - 2).collect();
        let work = WorkItem { model_id, tile_index, input: input.clone() };

        let gate = UtilityGate::new(2);
        let proof = reg.tile_proof(&model_id, tile_index).unwrap();
        let sol = gate.solve(n, &work, &easy_target(), weights[tile_index].clone(), proof);

        // The gate accepts a genuine model computation.
        assert_eq!(gate.verify(&reg, &work, &easy_target(), &sol), Ok(()));

        // The useful output equals the real layer result A·B.
        assert_eq!(gate.useful_output(&sol, n), gemm::matmul(&weights[tile_index], &input, n));
    }

    #[test]
    fn rejects_forged_random_weights() {
        // This is the Pearl attack: "AI-shaped" work with fabricated matrices.
        let n = 8;
        let mut reg = ModelRegistry::new();
        let weights = toy_model(n, 5);
        let model_id = reg.register("toy-llm", n, &weights);

        let tile_index = 1;
        let input: Vec<i64> = (0..n * n).map(|i| i as i64 % 3).collect();
        let work = WorkItem { model_id, tile_index, input };
        let gate = UtilityGate::new(2);

        // Miner fabricates random weights but reuses a real tile's proof.
        let fake_weights: Vec<i64> = (0..n * n).map(|i| (i * 13 % 9) as i64).collect();
        let real_proof = reg.tile_proof(&model_id, tile_index).unwrap();
        let sol = gate.solve(n, &work, &easy_target(), fake_weights, real_proof);

        // The Merkle check fails: forged weights are not the committed tile.
        assert_eq!(
            gate.verify(&reg, &work, &easy_target(), &sol),
            Err(GateError::ForgedWeights)
        );
    }

    #[test]
    fn rejects_wrong_input() {
        let n = 8;
        let mut reg = ModelRegistry::new();
        let weights = toy_model(n, 4);
        let model_id = reg.register("toy-llm", n, &weights);
        let tile_index = 0;
        let gate = UtilityGate::new(2);

        let requested = WorkItem {
            model_id,
            tile_index,
            input: (0..n * n).map(|i| i as i64 % 4).collect(),
        };
        // Miner solves for a DIFFERENT input than requested.
        let other = WorkItem {
            model_id,
            tile_index,
            input: (0..n * n).map(|_| 1i64).collect(),
        };
        let proof = reg.tile_proof(&model_id, tile_index).unwrap();
        let sol = gate.solve(n, &other, &easy_target(), weights[tile_index].clone(), proof);

        // Bound to the wrong work item → rejected.
        assert_eq!(
            gate.verify(&reg, &requested, &easy_target(), &sol),
            Err(GateError::WorkMismatch)
        );
    }

    #[test]
    fn merkle_roundtrips() {
        let n = 4;
        let tiles = toy_model(n, 6);
        let leaves = padded_leaves(tiles.iter().map(|t| tile_leaf(t)).collect());
        let root = merkle_root(&leaves);
        for i in 0..tiles.len() {
            let proof = merkle_proof(&leaves, i);
            assert!(merkle_verify(&root, &tile_leaf(&tiles[i]), &proof));
        }
        // A wrong leaf fails.
        let bad = merkle_proof(&leaves, 0);
        assert!(!merkle_verify(&root, &tile_leaf(&tiles[1]), &bad));
    }
}
