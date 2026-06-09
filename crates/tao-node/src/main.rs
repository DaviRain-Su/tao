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
        /// P2P listen address, e.g. 127.0.0.1:9001 (enables networking).
        #[arg(long)]
        listen: Option<String>,
        /// Comma-separated bootstrap peer addresses to dial.
        #[arg(long)]
        peers: Option<String>,
        /// Faucet keypair file (enables requestAirdrop; must be funded in genesis).
        #[arg(long)]
        faucet_keypair: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log);
    match cli.command {
        Command::Init { data_dir } => init(data_dir),
        Command::Run {
            config, data_dir, mine, miner, blocks, rpc, rpc_port, listen, peers, faucet_keypair,
        } => run(RunArgs {
            config, data_dir, mine, miner, blocks, rpc, rpc_port, listen, peers, faucet_keypair,
        }),
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

struct RunArgs {
    config: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    mine: bool,
    miner: Option<String>,
    blocks: u64,
    rpc: bool,
    rpc_port: Option<u16>,
    listen: Option<String>,
    peers: Option<String>,
    faucet_keypair: Option<PathBuf>,
}

/// Read a Solana-style keypair file (JSON array of 64 bytes).
fn load_keypair_bytes(path: &PathBuf) -> anyhow::Result<[u8; 64]> {
    let raw = std::fs::read_to_string(path)?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse keypair {}: {e}", path.display()))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("keypair {} must be 64 bytes", path.display()))
}

fn run(args: RunArgs) -> anyhow::Result<()> {
    let config = match args.config {
        Some(path) => NodeConfig::load(path)?,
        None => NodeConfig::default(),
    };
    let data_dir = args.data_dir.unwrap_or_else(|| config.data_dir.clone());
    let genesis_path = data_dir.join("genesis.toml");
    let genesis =
        if genesis_path.exists() { GenesisConfig::load(&genesis_path)? } else { GenesisConfig::devnet() };

    if !args.mine && !args.rpc && args.listen.is_none() {
        println!(
            "tao-node {} — network '{}'. Pass --mine and/or --rpc / --listen ADDR.",
            tao_core::VERSION, config.network
        );
        return Ok(());
    }

    // Miner reward address (only required when this node produces blocks).
    let miner_pubkey = if args.mine {
        let s = args
            .miner
            .or_else(|| config.miner.reward_address.clone())
            .ok_or_else(|| anyhow::anyhow!("--mine requires --miner <PUBKEY> or miner.reward_address"))?;
        Pubkey::from_str(&s).map_err(|e| anyhow::anyhow!("invalid miner address '{s}': {e}"))?
    } else {
        Pubkey::default()
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed)).ok();
    }

    let faucet = match &args.faucet_keypair {
        Some(path) => Some(load_keypair_bytes(path)?),
        None => None,
    };

    let (miner_loop, shared) = prepare(MineOptions {
        data_dir,
        genesis,
        miner: miner_pubkey,
        blocks: args.blocks,
        mine: args.mine,
        faucet,
    })?;

    // P2P networking.
    let (network, inbound_rx) = if let Some(listen_str) = args.listen {
        let listen_addr: SocketAddr =
            listen_str.parse().map_err(|e| anyhow::anyhow!("bad --listen address: {e}"))?;
        let peers: Vec<SocketAddr> = args
            .peers
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<SocketAddr>())
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("bad --peers address: {e}"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let net = tao_p2p::Network::start(listen_addr, peers, tx)
            .map_err(|e| anyhow::anyhow!("p2p start: {e}"))?;
        shared.attach_network(net.clone());
        (Some(net), Some(rx))
    } else {
        (None, None)
    };

    // RPC server in its own thread + runtime; the node loop runs on main.
    let rpc_thread = if args.rpc {
        let port = args.rpc_port.unwrap_or(config.rpc.port);
        let addr: SocketAddr = format!("{}:{}", config.rpc.bind, port)
            .parse()
            .map_err(|e| anyhow::anyhow!("bad rpc bind address: {e}"))?;
        let shared = shared.clone();
        let shutdown = shutdown.clone();
        Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(rpc::serve(shared, addr, shutdown)) {
                tracing::error!(error = %e, "rpc server error");
            }
        }))
    } else {
        None
    };

    miner_loop.run(shared, shutdown, network, inbound_rx)?;
    if let Some(t) = rpc_thread {
        let _ = t.join();
    }
    Ok(())
}
