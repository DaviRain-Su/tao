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

use tao_consensus::Blake3Pow;
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
        /// Use the matmul-PoUW matrix algorithm for PoW (AI-shaped, from tao-pouw).
        /// Combine with --matmul-n / --matmul-rank for size, or --pow-switch-height for automatic Blake3->matmul switch.
        #[arg(long)]
        matmul: bool,
        /// Matrix dimension n for matmul-PoUW (n x n matrices). Default 8 when --matmul is used.
        #[arg(long)]
        matmul_n: Option<usize>,
        /// Noise rank for matmul-PoUW. Default 2 when --matmul is used.
        #[arg(long)]
        matmul_rank: Option<usize>,
        /// Height at which to switch from Blake3 to matmul-PoUW (enables HeightSwitchPow).
        /// Before this height: Blake3. At/after: the matmul config from --matmul-* flags (or defaults).
        #[arg(long)]
        pow_switch_height: Option<u64>,
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
    /// Mine a single-node blockDAG (GHOSTDAG + reachability + SVM linearization).
    DagMine {
        #[arg(long, default_value = ".tao-dag")]
        data_dir: PathBuf,
        /// Base58 reward address for mined blocks.
        #[arg(long)]
        miner: String,
        /// Number of DAG blocks to mine.
        #[arg(long, default_value_t = 10)]
        blocks: u64,
        /// GHOSTDAG anticone bound k.
        #[arg(long, default_value_t = 18)]
        k: u16,
    },
    /// Run a networked blockDAG node (multi-miner gossip over TCP).
    DagRun {
        #[arg(long, default_value = ".tao-dag")]
        data_dir: PathBuf,
        /// Base58 reward address for mined blocks.
        #[arg(long)]
        miner: String,
        /// P2P listen address, e.g. 127.0.0.1:9101.
        #[arg(long)]
        listen: String,
        /// Comma-separated bootstrap peer addresses to dial.
        #[arg(long)]
        peers: Option<String>,
        /// Milliseconds between mined blocks.
        #[arg(long, default_value_t = 500)]
        block_interval_ms: u64,
        /// Stop after mining this many blocks. `0` = until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        blocks: u64,
        #[arg(long, default_value_t = 18)]
        k: u16,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log);
    match cli.command {
        Command::Init { data_dir } => init(data_dir),
        Command::Run {
            config, data_dir, mine, miner, blocks, rpc, rpc_port, listen, peers, faucet_keypair, matmul,
            matmul_n, matmul_rank, pow_switch_height,
        } => run(RunArgs {
            config, data_dir, mine, miner, blocks, rpc, rpc_port, listen, peers, faucet_keypair, matmul,
            matmul_n, matmul_rank, pow_switch_height,
        }),
        Command::DagMine { data_dir, miner, blocks, k } => dag_mine(data_dir, miner, blocks, k),
        Command::DagRun { data_dir, miner, listen, peers, block_interval_ms, blocks, k } => {
            dag_run(data_dir, miner, listen, peers, block_interval_ms, blocks, k)
        }
    }
}

/// Helper: build the DagChain config (genesis target + allocations) shared by
/// the dag-mine and dag-run commands.
fn dag_open(
    data_dir: PathBuf,
    miner: &str,
    k: u16,
) -> anyhow::Result<(tao_dagvm::DagChain, Pubkey)> {
    use std::str::FromStr;
    std::fs::create_dir_all(&data_dir)?;
    let genesis_path = data_dir.join("genesis.toml");
    let genesis = if genesis_path.exists() {
        GenesisConfig::load(&genesis_path)?
    } else {
        let g = GenesisConfig::devnet();
        std::fs::write(&genesis_path, g.to_toml()?)?;
        g
    };
    let miner_pubkey =
        Pubkey::from_str(miner).map_err(|e| anyhow::anyhow!("invalid miner '{miner}': {e}"))?;
    let target = tao_consensus::genesis::parse_target(&genesis.pow.initial_target)
        .map_err(|e| anyhow::anyhow!("bad genesis target: {e}"))?;
    let allocations: Vec<(Pubkey, u64)> = genesis
        .allocations
        .iter()
        .map(|a| {
            Pubkey::from_str(&a.address)
                .map(|pk| (pk, a.lamports))
                .map_err(|e| anyhow::anyhow!("bad allocation '{}': {e}", a.address))
        })
        .collect::<anyhow::Result<_>>()?;
    let chain = tao_dagvm::DagChain::open_with_daa(
        data_dir,
        k,
        target,
        miner_pubkey,
        genesis.reward.initial_lamports,
        allocations,
        genesis.pow.target_block_time_secs,
        genesis.pow.lwma_window,
    )
    .map_err(|e| anyhow::anyhow!("open dag chain: {e}"))?;
    Ok((chain, miner_pubkey))
}

/// A networked multi-miner blockDAG node: gossip DAG blocks over TCP, mine on
/// the current tips, accept peer blocks (buffering orphans until their parents
/// arrive), and converge with peers on one GHOSTDAG order.
#[allow(clippy::too_many_arguments)]
fn dag_run(
    data_dir: PathBuf,
    miner: String,
    listen: String,
    peers: Option<String>,
    block_interval_ms: u64,
    blocks: u64,
    k: u16,
) -> anyhow::Result<()> {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tao_dagvm::DagBlock;
    use tao_p2p::{NetMsg, Network};

    let (mut chain, miner_pubkey) = dag_open(data_dir, &miner, k)?;

    let listen_addr: SocketAddr = listen.parse().map_err(|e| anyhow::anyhow!("bad --listen: {e}"))?;
    let peer_addrs: Vec<SocketAddr> = peers
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<SocketAddr>())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("bad --peers: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let network = Network::start(listen_addr, peer_addrs, tx).map_err(|e| anyhow::anyhow!("p2p: {e}"))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed)).ok();
    }

    let mut pending: Vec<DagBlock> = Vec::new();
    let mut produced = 0u64;
    let mut last_mine = Instant::now() - Duration::from_millis(block_interval_ms);
    let mut last_log = Instant::now();
    let mut last_request = Instant::now() - Duration::from_secs(1);
    // Fire the first tip request immediately so a fresh node syncs on connect.
    let mut last_tipreq = Instant::now() - Duration::from_secs(10);

    tracing::info!(%listen, miner = %miner_pubkey, "blockDAG node started");

    while !shutdown.load(Ordering::Relaxed) {
        // Drain inbound: buffer announced blocks, serve backfill requests.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                NetMsg::NewBlock(bytes) => {
                    if let Ok(block) = bincode::deserialize::<DagBlock>(&bytes) {
                        pending.push(block);
                    }
                }
                NetMsg::GetBlock(id) => {
                    if let Some(block) = chain.get_block(&id) {
                        let bytes = bincode::serialize(&block).expect("serialize block");
                        network.broadcast(&NetMsg::NewBlock(bytes));
                    }
                }
                NetMsg::GetTips => {
                    network.broadcast(&NetMsg::Tips(chain.tips().to_vec()));
                }
                NetMsg::Tips(tips) => {
                    // Pull any tip we don't have → triggers transitive backfill.
                    for id in tips {
                        if !chain.has_block(&id) {
                            network.broadcast(&NetMsg::GetBlock(id));
                        }
                    }
                }
                NetMsg::NewTx(_) => {}
            }
        }
        // Apply pending blocks, retrying orphans until no further progress.
        loop {
            let mut progress = false;
            let mut still = Vec::new();
            for block in pending.drain(..) {
                match chain.accept(block.clone()) {
                    Ok(()) => progress = true,
                    Err(e) if e.contains("orphan") => still.push(block),
                    Err(_) => {} // invalid PoW / lowballed difficulty — drop
                }
            }
            pending = still;
            if !progress {
                break;
            }
        }

        // Backfill: for any still-orphaned block, request its missing ancestors
        // (throttled, re-requested until resolved so lost messages recover).
        if !pending.is_empty() && last_request.elapsed() >= Duration::from_millis(300) {
            let mut wanted = std::collections::HashSet::new();
            for block in &pending {
                for parent in &block.header.parents {
                    if !chain.has_block(parent) {
                        wanted.insert(*parent);
                    }
                }
            }
            for id in wanted {
                network.broadcast(&NetMsg::GetBlock(id));
            }
            last_request = Instant::now();
        }

        // Initial-sync heartbeat: ask peers for their tips so a node with no
        // fresh gossip still discovers and backfills the chain.
        if last_tipreq.elapsed() >= Duration::from_secs(2) {
            network.broadcast(&NetMsg::GetTips);
            last_tipreq = Instant::now();
        }

        // Mine on the current tips at the configured cadence.
        if last_mine.elapsed() >= Duration::from_millis(block_interval_ms) {
            let block = chain.mine(&[]).map_err(|e| anyhow::anyhow!("mine: {e}"))?;
            let bytes = bincode::serialize(&block).expect("serialize block");
            network.broadcast(&NetMsg::NewBlock(bytes));
            produced += 1;
            last_mine = Instant::now();
            if blocks != 0 && produced >= blocks {
                break;
            }
        }

        if last_log.elapsed() >= Duration::from_secs(2) {
            tracing::info!(
                blocks = chain.block_count(),
                tips = chain.tips().len(),
                order = chain.total_order().len(),
                peers = network.peer_count(),
                produced,
                "dag status"
            );
            last_log = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let bank = chain.rebuild_state().map_err(|e| anyhow::anyhow!("rebuild: {e}"))?;
    println!(
        "stopped: blocks={} tips={} order={} miner_balance={} state_root={}",
        chain.block_count(),
        chain.tips().len(),
        chain.total_order().len(),
        bank.balance(&miner_pubkey),
        hex_bytes(&bank.state_root().map_err(|e| anyhow::anyhow!("{e}"))?)
    );
    Ok(())
}

/// Mine a single-node blockDAG using the ported reachability + GHOSTDAG, with
/// state computed by executing the GHOSTDAG total order through the SVM.
fn dag_mine(data_dir: PathBuf, miner: String, blocks: u64, k: u16) -> anyhow::Result<()> {
    let (mut chain, miner_pubkey) = dag_open(data_dir, &miner, k)?;

    for _ in 0..blocks {
        chain.mine(&[]).map_err(|e| anyhow::anyhow!("mine: {e}"))?;
    }

    let bank = chain.rebuild_state().map_err(|e| anyhow::anyhow!("rebuild state: {e}"))?;
    let state_root = bank.state_root().map_err(|e| anyhow::anyhow!("state root: {e}"))?;
    let order = chain.total_order();

    println!("blockDAG mined:");
    println!("  blocks (incl. genesis): {}", chain.block_count());
    println!("  tips:                   {}", chain.tips().len());
    println!("  total-order length:     {}", order.len());
    println!("  miner balance:          {}", bank.balance(&miner_pubkey));
    println!("  state root:             {}", hex_bytes(&state_root));
    Ok(())
}

fn hex_bytes(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    matmul: bool,
    matmul_n: Option<usize>,
    matmul_rank: Option<usize>,
    pow_switch_height: Option<u64>,
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

    // Determine PoW algorithm from CLI flags.
    // Supports plain matmul, or automatic switch (HeightSwitchPow) for the M7 evolution story.
    let default_n = 8usize;
    let default_rank = 2usize;
    let use_matmul = args.matmul || args.matmul_n.is_some() || args.matmul_rank.is_some();
    let n = args.matmul_n.unwrap_or(default_n);
    let rank = args.matmul_rank.unwrap_or(default_rank);

    let pow: Arc<dyn tao_consensus::PowAlgorithm> = if let Some(switch_h) = args.pow_switch_height {
        // Blake3 (fair launch / CPU) until switch_h, then matmul-PoUW.
        let before = Arc::new(Blake3Pow);
        let after = Arc::new(tao_pouw::MatmulPow::new(n, rank));
        Arc::new(tao_consensus::HeightSwitchPow::new(before, after, switch_h))
    } else if use_matmul {
        // Pure matmul-PoUW (matrix multiplication as the PoW work).
        // Use small n/rank for CPU demos; larger values (e.g. 64,4) for GPU-like feel.
        Arc::new(tao_pouw::MatmulPow::new(n, rank))
    } else {
        Arc::new(Blake3Pow)
    };

    let (miner_loop, shared) = prepare(MineOptions {
        data_dir,
        genesis,
        miner: miner_pubkey,
        blocks: args.blocks,
        mine: args.mine,
        faucet,
        pow,
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
