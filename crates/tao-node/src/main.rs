//! `tao-node` — the Tao chain full-node daemon.
//!
//! M0 scaffold + M1 PoW + M2 SVM execution + M3 program deployment + M4
//! Solana-compatible JSON-RPC.

mod node;
mod rpc;
mod shared;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use solana_pubkey::Pubkey;
use tao_core::{config::NodeConfig, genesis::GenesisConfig, logging};

use node::{prepare, MineOptions};

#[derive(Parser)]
#[command(name = "tao-node", version, about = "Tao chain full node", long_about = None)]
struct Cli {
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
        #[arg(long)]
        config: Option<PathBuf>,
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
        /// Serve the Solana-compatible JSON-RPC.
        #[arg(long)]
        rpc: bool,
        /// RPC port (default from config, usually 8899).
        #[arg(long)]
        rpc_port: Option<u16>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log);
    match cli.command {
        Command::Init { data_dir } => init(data_dir),
        Command::Run { config, data_dir, mine, miner, blocks, rpc, rpc_port } => {
            run(config, data_dir, mine, miner, blocks, rpc, rpc_port)
        }
    }
}

fn init(data_dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(&data_dir)?;
    let mut config = NodeConfig::default();
    config.data_dir = data_dir.clone();
    std::fs::write(data_dir.join("config.toml"), config.to_toml()?)?;
    std::fs::write(data_dir.join("genesis.toml"), GenesisConfig::devnet().to_toml()?)?;
    println!("Initialized Tao data directory at {}", data_dir.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    config_path: Option<PathBuf>,
    data_dir_override: Option<PathBuf>,
    mine: bool,
    miner_arg: Option<String>,
    blocks: u64,
    rpc: bool,
    rpc_port: Option<u16>,
) -> anyhow::Result<()> {
    let config = match config_path {
        Some(path) => NodeConfig::load(path)?,
        None => NodeConfig::default(),
    };
    let data_dir = data_dir_override.unwrap_or_else(|| config.data_dir.clone());
    let genesis_path = data_dir.join("genesis.toml");
    let genesis =
        if genesis_path.exists() { GenesisConfig::load(&genesis_path)? } else { GenesisConfig::devnet() };

    if !mine {
        println!(
            "tao-node {} — network '{}'. Pass --mine [--rpc] to run.",
            tao_core::VERSION, config.network
        );
        return Ok(());
    }

    let miner_str = miner_arg
        .or_else(|| config.miner.reward_address.clone())
        .ok_or_else(|| anyhow::anyhow!("--mine requires --miner <PUBKEY> or miner.reward_address"))?;
    let miner = Pubkey::from_str(&miner_str)
        .map_err(|e| anyhow::anyhow!("invalid miner address '{miner_str}': {e}"))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed)).ok();
    }

    let (miner_loop, shared) =
        prepare(MineOptions { data_dir, genesis, miner, blocks })?;

    if rpc {
        let port = rpc_port.unwrap_or(config.rpc.port);
        let addr: SocketAddr = format!("{}:{}", config.rpc.bind, port)
            .parse()
            .map_err(|e| anyhow::anyhow!("bad rpc bind address: {e}"))?;
        // RPC server runs in its own thread with a tokio runtime; the miner
        // runs on the main thread (no Send requirement on the Bank).
        let rpc_thread = {
            let shared = shared.clone();
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                if let Err(e) = rt.block_on(rpc::serve(shared, addr, shutdown)) {
                    tracing::error!(error = %e, "rpc server error");
                }
            })
        };
        miner_loop.run(shared, shutdown)?;
        let _ = rpc_thread.join();
    } else {
        miner_loop.run(shared, shutdown)?;
    }
    Ok(())
}
