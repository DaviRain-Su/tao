//! Node runtime configuration.
//!
//! Loaded from a TOML file (see [`NodeConfig::load`]) or constructed with
//! sensible devnet defaults via [`NodeConfig::default`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TaoError};

/// Top-level node configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Directory for chain state, account DB, and keys.
    pub data_dir: PathBuf,
    /// Network name; must match the genesis `network`.
    pub network: String,
    pub rpc: RpcConfig,
    pub p2p: P2pConfig,
    pub miner: MinerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcConfig {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct P2pConfig {
    pub bind: String,
    pub port: u16,
    /// Bootstrap peer multiaddrs to dial on startup.
    pub bootstrap: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MinerConfig {
    pub enabled: bool,
    /// Base58 address that receives coinbase rewards. Required when mining.
    pub reward_address: Option<String>,
    /// Worker threads for the CPU (RandomX) miner. `0` = number of CPUs.
    pub threads: usize,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(".tao"),
            network: "tao-devnet".to_string(),
            rpc: RpcConfig::default(),
            p2p: P2pConfig::default(),
            miner: MinerConfig::default(),
        }
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".to_string(),
            port: 8899, // Solana-compatible default RPC port
        }
    }
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 9001,
            bootstrap: Vec::new(),
        }
    }
}

impl Default for MinerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reward_address: None,
            threads: 0,
        }
    }
}

impl NodeConfig {
    /// Load configuration from a TOML file path.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        toml::from_str(&raw).map_err(|e| TaoError::Config(e.to_string()))
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| TaoError::Config(e.to_string()))
    }
}
