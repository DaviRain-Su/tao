//! `tao-node` — the Tao chain full-node daemon.
//!
//! M0 scaffold + M1 single-node CPU miner. Networking, SVM execution, and RPC
//! are filled in by later milestones.

mod node;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use solana_pubkey::Pubkey;
use tao_core::{config::NodeConfig, genesis::GenesisConfig, logging};

use node::{run_mining, MineOptions};

#[derive(Parser)]
#[command(name = "tao-node", version, about = "Tao chain full node", long_about = None)]
struct Cli {
    /// Log directive when RUST_LOG is unset.
    #[arg(long, global = true, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a default config + devnet genesis into a data directory.
    Init {
        #[arg(long, default_value = ".tao")]
        data_dir: PathBuf,
    },
    /// Run the node.
    Run {
        /// Path to a node config TOML. Defaults to built-in devnet config.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the data directory (else taken from config).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Enable the CPU miner.
        #[arg(long)]
        mine: bool,
        /// Base58 reward address for mined blocks (overrides config).
        #[arg(long)]
        miner: Option<String>,
        /// Stop after mining this many blocks. `0` = run until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        blocks: u64,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log);

    match cli.command {
        Command::Init { data_dir } => init(data_dir),
        Command::Run { config, data_dir, mine, miner, blocks } => {
            run(config, data_dir, mine, miner, blocks)
        }
    }
}

fn init(data_dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(&data_dir)?;

    let mut config = NodeConfig::default();
    config.data_dir = data_dir.clone();
    let config_path = data_dir.join("config.toml");
    std::fs::write(&config_path, config.to_toml()?)?;

    let genesis = GenesisConfig::devnet();
    let genesis_path = data_dir.join("genesis.toml");
    std::fs::write(&genesis_path, genesis.to_toml()?)?;

    tracing::info!(?config_path, ?genesis_path, "initialized data directory");
    println!("Initialized Tao data directory at {}", data_dir.display());
    println!("  config:  {}", config_path.display());
    println!("  genesis: {}", genesis_path.display());
    Ok(())
}

fn run(
    config_path: Option<PathBuf>,
    data_dir_override: Option<PathBuf>,
    mine: bool,
    miner_arg: Option<String>,
    blocks: u64,
) -> anyhow::Result<()> {
    let config = match config_path {
        Some(path) => NodeConfig::load(path)?,
        None => NodeConfig::default(),
    };
    let data_dir = data_dir_override.unwrap_or_else(|| config.data_dir.clone());

    // Genesis: prefer the on-disk genesis, else the built-in devnet genesis.
    let genesis_path = data_dir.join("genesis.toml");
    let genesis = if genesis_path.exists() {
        GenesisConfig::load(&genesis_path)?
    } else {
        GenesisConfig::devnet()
    };

    if !mine {
        tracing::info!(
            network = %config.network,
            rpc_port = config.rpc.port,
            p2p_port = config.p2p.port,
            "tao-node starting (no miner; networking/RPC land in M4/M5)"
        );
        println!(
            "tao-node {} — network '{}'. Pass --mine to start the CPU miner.",
            tao_core::VERSION,
            config.network
        );
        return Ok(());
    }

    // Resolve the miner reward address.
    let miner_str = miner_arg
        .or_else(|| config.miner.reward_address.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("--mine requires --miner <PUBKEY> or miner.reward_address in config")
        })?;
    let miner = Pubkey::from_str(&miner_str)
        .map_err(|e| anyhow::anyhow!("invalid miner address '{miner_str}': {e}"))?;

    // Graceful shutdown on Ctrl-C.
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::Relaxed);
        })
        .ok();
    }

    run_mining(
        MineOptions { data_dir, genesis, miner, blocks },
        shutdown,
    )
}
