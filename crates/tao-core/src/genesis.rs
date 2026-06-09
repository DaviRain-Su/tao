//! Genesis configuration format.
//!
//! The genesis file pins the initial chain parameters every node must agree on:
//! network name, initial PoW difficulty, emission schedule, and the initial
//! account allocations (premine / faucet funding).

use serde::{Deserialize, Serialize};

use crate::error::{Result, TaoError};

/// The genesis configuration shared by all nodes on a network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Network identifier; must match `NodeConfig::network`.
    pub network: String,
    /// Genesis block timestamp (unix seconds).
    pub creation_time: i64,
    /// PoW parameters.
    pub pow: PowParams,
    /// Coinbase emission schedule.
    pub reward: RewardParams,
    /// Initial account balances.
    #[serde(default)]
    pub allocations: Vec<Allocation>,
    /// Optional utility-gated matmul-PoUW model committed by genesis. When set,
    /// `tao-node run --pouw` mines this exact model's layers (all nodes derive the
    /// same weights from `weight_seed`, so the model id is consensus-agreed).
    #[serde(default)]
    pub pouw: Option<PouwModelParams>,
}

/// Genesis-committed utility-gate model. The weights are derived deterministically
/// from `weight_seed` (so every node agrees on the exact model without shipping
/// the full weights), and `model_id` — if set — pins the expected Merkle
/// commitment for a cross-check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PouwModelParams {
    /// Human-readable model name (part of the model id commitment).
    pub name: String,
    /// Matrix dimension (n×n weight tiles and inputs).
    pub n: usize,
    /// Low-rank noise rank for the matmul puzzle.
    pub rank: usize,
    /// Number of weight tiles (layers).
    pub tiles: usize,
    /// Hex-encoded 32-byte seed the weights are derived from.
    pub weight_seed: String,
    /// Optional hex-encoded expected model id (Merkle commitment) for cross-check.
    #[serde(default)]
    pub model_id: Option<String>,
}

/// Proof-of-work parameters for the launch (RandomX/CPU) phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowParams {
    /// Target seconds per block.
    pub target_block_time_secs: u64,
    /// LWMA difficulty window size (blocks).
    pub lwma_window: u64,
    /// Initial difficulty target as a compact value (higher = easier).
    /// Stored as a hex-encoded 256-bit big-endian target threshold.
    pub initial_target: String,
    /// Consensus PoW algorithm: `"blake3"` (default), `"matmul"`, or `"pouw"`
    /// (the genesis `[pouw]` model is the consensus PoW). Committed into the
    /// genesis id via [`GenesisConfig::commitment`], so nodes with different
    /// algorithms cannot silently share a chain.
    #[serde(default = "default_pow_algorithm")]
    pub algorithm: String,
    /// Optional hard-fork height for a Blake3 → `algorithm` switch
    /// (`HeightSwitchPow`). `None` runs `algorithm` from genesis.
    #[serde(default)]
    pub switch_height: Option<u64>,
}

fn default_pow_algorithm() -> String {
    "blake3".to_string()
}

/// Coinbase emission schedule (Bitcoin-style halving for the MVP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardParams {
    /// Initial block reward in the smallest unit (lamports).
    pub initial_lamports: u64,
    /// Number of blocks between halvings. `0` disables halving.
    pub halving_interval: u64,
}

/// A single initial balance assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation {
    /// Base58-encoded Solana-compatible address.
    pub address: String,
    /// Balance in lamports.
    pub lamports: u64,
}

impl GenesisConfig {
    /// Load a genesis config from a TOML file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        toml::from_str(&raw).map_err(|e| TaoError::Genesis(e.to_string()))
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| TaoError::Genesis(e.to_string()))
    }

    /// The genesis **commitment**: a 32-byte hash over the entire canonical
    /// (bincode) encoding of this config. Committed into the genesis block id,
    /// so two nodes whose genesis files differ in *any* consensus parameter
    /// (allocations, reward schedule, PoW algorithm, pouw model, …) derive
    /// different genesis ids and refuse to share a chain — instead of silently
    /// forking on the first state-root mismatch.
    pub fn commitment(&self) -> [u8; 32] {
        let bytes = bincode::serialize(self).expect("genesis config serializes");
        *blake3::hash(&bytes).as_bytes()
    }

    /// A built-in devnet genesis for local development.
    pub fn devnet() -> Self {
        Self {
            network: "tao-devnet".to_string(),
            creation_time: 1_750_000_000, // fixed for determinism across nodes
            pow: PowParams {
                target_block_time_secs: 10,
                lwma_window: 90,
                // Easy starting target (top byte zero) for CPU mining on a laptop.
                initial_target: "00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .to_string(),
                algorithm: default_pow_algorithm(),
                switch_height: None,
            },
            reward: RewardParams {
                initial_lamports: 1_000_000_000, // 1 TAO (9 decimals, Solana-style)
                halving_interval: 2_100_000,
            },
            allocations: Vec::new(),
            pouw: Some(PouwModelParams {
                name: "tao-devnet-pouw".to_string(),
                n: 8,
                rank: 2,
                tiles: 8,
                // Fixed seed → deterministic weights → consensus-agreed model id.
                weight_seed:
                    "1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
                model_id: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_is_sensitive_to_every_consensus_parameter() {
        let base = GenesisConfig::devnet();
        let c0 = base.commitment();
        assert_eq!(c0, GenesisConfig::devnet().commitment(), "deterministic");

        let mut g = base.clone();
        g.allocations.push(Allocation {
            address: "11111111111111111111111111111111".into(),
            lamports: 1,
        });
        assert_ne!(g.commitment(), c0, "allocations are committed");

        let mut g = base.clone();
        g.reward.initial_lamports += 1;
        assert_ne!(g.commitment(), c0, "reward schedule is committed");

        let mut g = base.clone();
        g.pow.algorithm = "pouw".into();
        assert_ne!(g.commitment(), c0, "pow algorithm is committed");

        let mut g = base.clone();
        if let Some(p) = &mut g.pouw {
            p.rank += 1;
        }
        assert_ne!(g.commitment(), c0, "pouw model params are committed");
    }

    #[test]
    fn devnet_genesis_toml_round_trips_with_pouw() {
        let g = GenesisConfig::devnet();
        let toml = g.to_toml().unwrap();
        assert!(toml.contains("[pouw]"), "pouw model serialized: {toml}");
        let back: GenesisConfig = toml::from_str(&toml).unwrap();
        let p = back.pouw.expect("pouw model preserved through TOML");
        assert_eq!(p.name, "tao-devnet-pouw");
        assert_eq!((p.n, p.rank, p.tiles), (8, 2, 8));
        assert_eq!(p.weight_seed.len(), 64);
    }
}
