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

    /// A built-in devnet genesis for local development.
    pub fn devnet() -> Self {
        Self {
            network: "tao-devnet".to_string(),
            creation_time: 1_750_000_000, // fixed for determinism across nodes
            pow: PowParams {
                target_block_time_secs: 10,
                lwma_window: 90,
                // Easy starting target (top byte zero) for CPU mining on a laptop.
                initial_target:
                    "00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                        .to_string(),
            },
            reward: RewardParams {
                initial_lamports: 1_000_000_000, // 1 TAO (9 decimals, Solana-style)
                halving_interval: 2_100_000,
            },
            allocations: Vec::new(),
        }
    }
}
