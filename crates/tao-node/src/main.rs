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
        /// Use the utility-gated matmul-PoUW as the chain's block PoW: the work is
        /// the genesis-committed model's layer (not free matrices). Overrides
        /// --matmul. The model (size, tiles, weights) comes from genesis `[pouw]`.
        #[arg(long)]
        pouw: bool,
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
        /// Finality depth: blocks kept beyond the checkpoint before finalized
        /// transaction bodies are pruned.
        #[arg(long, default_value_t = 100)]
        finality_depth: u64,
        /// Serve the Solana-compatible JSON-RPC (so web3.js / wallets can query
        /// state and submit transactions that get mined into DAG blocks).
        #[arg(long)]
        rpc: bool,
        /// RPC port (default 8899).
        #[arg(long)]
        rpc_port: Option<u16>,
    },
    /// Run the M7b utility-gated matmul-PoUW miner: register a model, mine
    /// model-bound matmul solutions (the work IS a real model layer applied to a
    /// requested input), verify each against the model's Merkle commitment, and
    /// emit the useful inference outputs.
    UtilityMine {
        /// Blocks (work items) to mine.
        #[arg(long, default_value_t = 8)]
        blocks: u64,
        /// Matrix dimension n (n×n weight tiles and inputs).
        #[arg(long, default_value_t = 8)]
        n: usize,
        /// Low-rank noise rank for the matmul-PoUW puzzle.
        #[arg(long, default_value_t = 2)]
        rank: usize,
        /// Number of weight tiles (layers) in the demo model.
        #[arg(long, default_value_t = 8)]
        tiles: usize,
        /// Optional JSON file of inference requests to serve from a queue:
        /// `[{"tile": <usize>, "input": [<n*n i64>]}, ...]`. Each request is mined
        /// (bound to the model), verified, and its real output `A·B` produced.
        /// Without it, inputs are generated (the demo). Overrides --blocks.
        #[arg(long)]
        requests: Option<PathBuf>,
        /// Optional path to write the inference results as JSON.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(&cli.log);
    match cli.command {
        Command::Init { data_dir } => init(data_dir),
        Command::Run {
            config,
            data_dir,
            mine,
            miner,
            blocks,
            rpc,
            rpc_port,
            listen,
            peers,
            faucet_keypair,
            matmul,
            matmul_n,
            matmul_rank,
            pow_switch_height,
            pouw,
        } => run(RunArgs {
            config,
            data_dir,
            mine,
            miner,
            blocks,
            rpc,
            rpc_port,
            listen,
            peers,
            faucet_keypair,
            matmul,
            matmul_n,
            matmul_rank,
            pow_switch_height,
            pouw,
        }),
        Command::DagMine {
            data_dir,
            miner,
            blocks,
            k,
        } => dag_mine(data_dir, miner, blocks, k),
        Command::DagRun {
            data_dir,
            miner,
            listen,
            peers,
            block_interval_ms,
            blocks,
            k,
            finality_depth,
            rpc,
            rpc_port,
        } => dag_run(DagRunArgs {
            data_dir,
            miner,
            listen,
            peers,
            block_interval_ms,
            blocks,
            k,
            finality_depth,
            rpc,
            rpc_port,
        }),
        Command::UtilityMine { blocks, n, rank, tiles, requests, out } => {
            utility_mine(blocks, n, rank, tiles, requests, out)
        }
    }
}

/// M7b utility-gated matmul-PoUW: register a model with a Merkle commitment over
/// its weight tiles, then for each block derive a work item (a real input applied
/// to one of the model's layers), mine a model-bound solution (grinding the noise
/// nonce until the PoW target is met), verify it against the commitment, and emit
/// the useful inference output `A·B`. A miner using random/forged weights is
/// rejected by the Merkle check — the work is provably a real model computation.
/// One queued inference request: apply model layer `tile` to `input` (n×n).
#[derive(serde::Deserialize)]
struct InferenceRequest {
    tile: usize,
    input: Vec<i64>,
}

/// A served result: the request's real output `A·B` plus its PoW nonce.
#[derive(serde::Serialize)]
struct InferenceResult {
    tile: usize,
    nonce: u64,
    output: Vec<i64>,
}

fn utility_mine(
    blocks: u64,
    n: usize,
    rank: usize,
    tiles: usize,
    requests: Option<PathBuf>,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    use tao_pouw::utility_gate::{ModelRegistry, UtilityGate, WorkItem};

    if n == 0 || rank == 0 || rank > n || tiles == 0 {
        return Err(anyhow::anyhow!("invalid dimensions: need 0<rank<=n, n>0, tiles>0"));
    }

    // Register a deterministic demo model (its id commits to the weights).
    let weights: Vec<Vec<i64>> = (0..tiles)
        .map(|t| (0..n * n).map(|i| ((t * 7 + i * 3) % 17) as i64 - 8).collect())
        .collect();
    let mut registry = ModelRegistry::new();
    let model_id = registry.register("tao-demo-llm", n, &weights);
    let gate = UtilityGate::new(rank);

    // A few bits of PoW so the CPU prototype mines quickly.
    let mut target = [0xffu8; 32];
    target[0] = 0x00;

    // Build the work queue: real requests from a file (the demand side), or
    // generated inputs for the demo.
    let queue: Vec<(usize, Vec<i64>)> = if let Some(path) = &requests {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read requests {}: {e}", path.display()))?;
        let reqs: Vec<InferenceRequest> = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse requests: {e}"))?;
        for (i, r) in reqs.iter().enumerate() {
            if r.tile >= tiles {
                return Err(anyhow::anyhow!("request {i}: tile {} out of range (tiles={tiles})", r.tile));
            }
            if r.input.len() != n * n {
                return Err(anyhow::anyhow!("request {i}: input len {} != n*n {}", r.input.len(), n * n));
            }
        }
        reqs.into_iter().map(|r| (r.tile, r.input)).collect()
    } else {
        (0..blocks)
            .map(|h| {
                let tile = (h as usize) % tiles;
                let input = (0..n * n).map(|i| ((h as usize + i) % 5) as i64 - 2).collect();
                (tile, input)
            })
            .collect()
    };

    println!("utility-gated matmul-PoUW:");
    println!("  model:     tao-demo-llm  id={}", hex_bytes(&model_id));
    println!("  dims:      {n}x{n}  tiles={tiles}  rank={rank}");
    println!(
        "  queue:     {} {}",
        queue.len(),
        if requests.is_some() { "request(s) from file" } else { "generated work item(s)" }
    );

    let mut total_nonces: u128 = 0;
    let mut output_checksum: i64 = 0;
    let mut results: Vec<InferenceResult> = Vec::with_capacity(queue.len());
    for (idx, (tile_index, input)) in queue.into_iter().enumerate() {
        let work = WorkItem { model_id, tile_index, input };
        let proof = registry
            .tile_proof(&model_id, tile_index)
            .ok_or_else(|| anyhow::anyhow!("missing tile proof"))?;
        let sol = gate.solve(n, &work, &target, weights[tile_index].clone(), proof);

        // A validating peer re-checks the binding (model + input + Merkle + PoW).
        gate.verify(&registry, &work, &target, &sol)
            .map_err(|e| anyhow::anyhow!("utility gate rejected request {idx}: {e:?}"))?;

        let output = gate.useful_output(&sol, n);
        total_nonces += sol.nonce as u128 + 1;
        output_checksum = output_checksum.wrapping_add(output.iter().sum::<i64>());
        results.push(InferenceResult { tile: tile_index, nonce: sol.nonce, output });
        tracing::info!(request = idx, tile = tile_index, nonce = sol.nonce, "served + verified inference");
    }

    let served = results.len();
    println!("  served:    {served}  (all verified against the model commitment)");
    println!("  avg grind: {} nonces/request", if served > 0 { total_nonces / served as u128 } else { 0 });
    println!("  output Σ:  {output_checksum}  (real A·B inference results)");

    if let Some(path) = &out {
        let json = serde_json::to_string_pretty(&results)?;
        std::fs::write(path, json).map_err(|e| anyhow::anyhow!("write out {}: {e}", path.display()))?;
        println!("  wrote {} results → {}", served, path.display());
    }
    Ok(())
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
    let chain = tao_dagvm::DagChain::open_with_genesis(
        data_dir,
        k,
        target,
        miner_pubkey,
        genesis.reward.initial_lamports,
        allocations,
        genesis.pow.target_block_time_secs,
        genesis.pow.lwma_window,
        // Commit the full genesis config into the genesis id: nodes with
        // mismatched genesis files derive different ids and reject each other's
        // blocks outright (no silent state-root fork).
        genesis.commitment(),
    )
    .map_err(|e| anyhow::anyhow!("open dag chain: {e}"))?;
    Ok((chain, miner_pubkey))
}

struct DagRunArgs {
    data_dir: PathBuf,
    miner: String,
    listen: String,
    peers: Option<String>,
    block_interval_ms: u64,
    blocks: u64,
    k: u16,
    finality_depth: u64,
    rpc: bool,
    rpc_port: Option<u16>,
}

/// A networked multi-miner blockDAG node: gossip DAG blocks over TCP, mine on
/// the current tips (draining the mempool), accept peer blocks (buffering orphans
/// until their parents arrive), and converge with peers on one GHOSTDAG order.
/// Optionally serves the Solana-compatible JSON-RPC over the DAG's SVM state.
fn dag_run(args: DagRunArgs) -> anyhow::Result<()> {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};
    use tao_dagvm::DagBlock;
    use tao_p2p::{InboundMsg, NetMsg, Network};

    let DagRunArgs {
        data_dir,
        miner,
        listen,
        peers,
        block_interval_ms,
        blocks,
        k,
        finality_depth,
        rpc,
        rpc_port,
    } = args;

    let (mut chain, miner_pubkey) = dag_open(data_dir, &miner, k)?;
    let requested_finality_depth = finality_depth;
    let finality_depth = requested_finality_depth.max(1);
    if finality_depth != requested_finality_depth {
        tracing::warn!(
            requested = requested_finality_depth,
            clamped = finality_depth,
            "invalid --finality-depth, using minimum of 1"
        );
    }
    chain.set_finality_depth(finality_depth);

    let listen_addr: SocketAddr = listen
        .parse()
        .map_err(|e| anyhow::anyhow!("bad --listen: {e}"))?;
    let peer_addrs: Vec<SocketAddr> = peers
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<SocketAddr>())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("bad --peers: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let network =
        Network::start(listen_addr, peer_addrs, tx).map_err(|e| anyhow::anyhow!("p2p: {e}"))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || shutdown.store(true, Ordering::Relaxed)).ok();
    }

    // RPC state: the DAG node serves Solana-compatible RPC over its SVM virtual
    // state. The mempool collects sendTransaction'd txs that the miner drains into
    // blocks; reads (getBalance/getAccountInfo) go to the current virtual state.
    use crate::shared::Shared;
    let genesis_hash = chain.total_order().first().copied().unwrap_or([0u8; 32]);
    let init_accounts = chain
        .virtual_state()
        .map_err(|e| anyhow::anyhow!("init virtual state: {e}"))?
        .accounts_arc();
    let height0 = chain.total_order().len().saturating_sub(1) as u64;
    // env_blockhash the DAG SVM executes against; getLatestBlockhash returns it
    // (the Bank does not enforce blockhash freshness, so any tx referencing it is
    // accepted).
    let env_blockhash = [7u8; 32];
    let shared = Arc::new(Shared::new(init_accounts, genesis_hash, height0, env_blockhash, None));
    // Attach the gossip network so RPC-submitted transactions propagate to peers
    // (any node can then mine them), not just the receiving node.
    shared.attach_network(network.clone());
    let rpc_thread = if rpc {
        let port = rpc_port.unwrap_or(8899);
        let addr: SocketAddr = format!("0.0.0.0:{port}")
            .parse()
            .map_err(|e| anyhow::anyhow!("bad rpc bind: {e}"))?;
        let shared = shared.clone();
        let shutdown = shutdown.clone();
        tracing::info!(%addr, "serving Solana-compatible JSON-RPC over the blockDAG");
        Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(rpc::serve(shared, addr, shutdown)) {
                tracing::error!(error = %e, "rpc server error");
            }
        }))
    } else {
        None
    };

    let mut pending: Vec<DagBlock> = Vec::new();
    // Seen-transaction set for gossip dedup (so relayed txs don't loop forever).
    let mut seen_tx: std::collections::HashSet<[u8; 64]> = std::collections::HashSet::new();
    let mut produced = 0u64;
    let mut last_mine = Instant::now() - Duration::from_millis(block_interval_ms);
    let mut last_log = Instant::now();
    let mut last_request = Instant::now() - Duration::from_secs(1);
    let mut last_snapshot_request = Instant::now() - Duration::from_secs(60);
    // Fire the first tip request immediately so a fresh node syncs on connect.
    let mut last_tipreq = Instant::now() - Duration::from_secs(10);
    let mut last_maint = Instant::now();

    tracing::info!(%listen, miner = %miner_pubkey, "blockDAG node started");

    while !shutdown.load(Ordering::Relaxed) {
        // Drain inbound: buffer announced blocks, serve backfill requests.
        while let Ok(msg) = rx.try_recv() {
            let InboundMsg { from, msg } = msg;
            match msg {
                NetMsg::NewBlock(bytes) => {
                    if let Ok(block) = DagBlock::from_bytes(&bytes) {
                        pending.push(block);
                    }
                }
                NetMsg::GetBlock(id) => {
                    if let Some(block) = chain.get_block(&id) {
                        let bytes = bincode::serialize(&block).expect("serialize block");
                        if let Err(e) = network.send_to(from, &NetMsg::NewBlock(bytes)) {
                            tracing::warn!(error = %e, "send NewBlock response failed");
                        }
                    }
                }
                NetMsg::GetTips => {
                    if let Err(e) = network.send_to(from, &NetMsg::Tips(chain.tips().to_vec())) {
                        tracing::warn!(error = %e, "send tips response failed");
                    }
                }
                NetMsg::Tips(tips) => {
                    // Pull any tip we don't have → triggers transitive backfill.
                    for id in tips {
                        if !chain.has_block(&id) {
                            network.broadcast(&NetMsg::GetBlock(id));
                        }
                    }
                }
                NetMsg::GetSnapshot => {
                    if let Some(snap) = chain.export_snapshot() {
                        if let Err(e) = network.send_to(from, &NetMsg::Snapshot(snap)) {
                            tracing::warn!(error = %e, "send snapshot response failed");
                        }
                    }
                }
                NetMsg::Snapshot(bytes) => match chain.import_snapshot(&bytes) {
                    Ok(true) => {
                        tracing::info!(blocks = chain.block_count(), "bootstrapped from snapshot")
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "snapshot import failed"),
                },
                NetMsg::NewTx(bytes) => {
                    // Gossip a transaction into the mempool, deduplicated, and
                    // flood-relay it once so it reaches the whole network (not
                    // just direct peers).
                    if let Ok(tx) = bincode::deserialize::<solana_transaction::Transaction>(&bytes) {
                        if let Some(sig) =
                            tx.signatures.first().and_then(|s| <[u8; 64]>::try_from(s.as_ref()).ok())
                        {
                            if seen_tx.insert(sig) {
                                shared.submit(tx);
                                network.broadcast(&NetMsg::NewTx(bytes));
                            }
                        }
                    }
                }
            }
        }
        // Apply pending blocks, retrying orphans until no further progress.
        // Newly-accepted peer blocks are flood-relayed so they reach nodes that
        // aren't our direct peers (multi-hop gossip; receivers dedup by has_block).
        let mut relay_blocks: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut progress = false;
            let mut still = Vec::new();
            for block in pending.drain(..) {
                match chain.accept(block.clone()) {
                    Ok(()) => {
                        progress = true;
                        relay_blocks.push(bincode::serialize(&block).expect("serialize block"));
                    }
                    Err(e) if e.contains("orphan") => still.push(block),
                    Err(e) if e.contains("pruned") => {}
                    Err(_) => {} // invalid PoW / lowballed difficulty — drop
                }
            }
            pending = still;
            if !progress {
                break;
            }
        }
        for b in relay_blocks {
            network.broadcast(&NetMsg::NewBlock(b));
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
            // Orphans we can't resolve may have pruned ancestors — ask one peer
            // for a snapshot, rate-limited to protect weak peers.
            if last_snapshot_request.elapsed() >= Duration::from_secs(60) {
                if let Some(peer) = network.any_peer() {
                    if let Err(e) = network.send_to(peer, &NetMsg::GetSnapshot) {
                        tracing::warn!(error = %e, "request snapshot failed");
                    } else {
                        last_snapshot_request = Instant::now();
                    }
                }
            }
            last_request = Instant::now();
        }

        // Initial-sync heartbeat: ask peers for their tips so a node with no
        // fresh gossip still discovers and backfills the chain.
        if last_tipreq.elapsed() >= Duration::from_secs(2) {
            network.broadcast(&NetMsg::GetTips);
            last_tipreq = Instant::now();
        }

        // Maintenance: advance the virtual state (forming checkpoints), then
        // re-anchor at a finalized pruning point to bound memory (headers +
        // GHOSTDAG data + tx bodies). No-op until the chain is deep enough.
        if last_maint.elapsed() >= Duration::from_secs(5) {
            if let Err(e) = chain.virtual_state() {
                tracing::warn!(error = %e, "virtual state failed during maintenance; skipping prune this cycle");
            } else if let Ok(n) = chain.prune() {
                if n > 0 {
                    tracing::info!(
                        pruned = n,
                        blocks = chain.block_count(),
                        "re-anchored / pruned"
                    );
                }
            }
            last_maint = Instant::now();
        }

        // Mine on the current tips at the configured cadence, including any
        // mempool transactions (from RPC sendTransaction).
        if last_mine.elapsed() >= Duration::from_millis(block_interval_ms) {
            let txs = shared.drain_mempool();
            let block = chain.mine(&txs).map_err(|e| anyhow::anyhow!("mine: {e}"))?;
            let bytes = bincode::serialize(&block).expect("serialize block");
            network.broadcast(&NetMsg::NewBlock(bytes));
            produced += 1;

            // Refresh RPC state: re-point reads at the new virtual state, advance
            // the head, and confirm the included transactions.
            match chain.virtual_state() {
                Ok(bank) => shared.set_accounts(bank.accounts_arc()),
                Err(e) => tracing::warn!(error = %e, "virtual state after mine"),
            }
            shared.advance(chain.total_order().len().saturating_sub(1) as u64, env_blockhash);
            for tx in &txs {
                if let Some(sig) = tx.signatures.first().and_then(|s| <[u8; 64]>::try_from(s.as_ref()).ok()) {
                    shared.confirm(sig, None);
                }
            }

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

    let (balance, root) = {
        let bank = chain
            .virtual_state()
            .map_err(|e| anyhow::anyhow!("virtual state: {e}"))?;
        (
            bank.balance(&miner_pubkey),
            bank.state_root().map_err(|e| anyhow::anyhow!("{e}"))?,
        )
    };
    println!(
        "stopped: blocks={} tips={} order={} miner_balance={} state_root={}",
        chain.block_count(),
        chain.tips().len(),
        chain.total_order().len(),
        balance,
        hex_bytes(&root)
    );
    if let Some(t) = rpc_thread {
        let _ = t.join();
    }
    Ok(())
}

/// Mine a single-node blockDAG using the ported reachability + GHOSTDAG, with
/// state computed by executing the GHOSTDAG total order through the SVM.
fn dag_mine(data_dir: PathBuf, miner: String, blocks: u64, k: u16) -> anyhow::Result<()> {
    let (mut chain, miner_pubkey) = dag_open(data_dir, &miner, k)?;

    for _ in 0..blocks {
        chain.mine(&[]).map_err(|e| anyhow::anyhow!("mine: {e}"))?;
    }

    let (balance, state_root) = {
        let bank = chain
            .virtual_state()
            .map_err(|e| anyhow::anyhow!("virtual state: {e}"))?;
        (
            bank.balance(&miner_pubkey),
            bank.state_root()
                .map_err(|e| anyhow::anyhow!("state root: {e}"))?,
        )
    };
    let order_len = chain.total_order().len();

    println!("blockDAG mined:");
    println!("  blocks (incl. genesis): {}", chain.block_count());
    println!("  tips:                   {}", chain.tips().len());
    println!("  total-order length:     {order_len}");
    println!("  miner balance:          {balance}");
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
    std::fs::write(
        data_dir.join("genesis.toml"),
        GenesisConfig::devnet().to_toml()?,
    )?;
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
    pouw: bool,
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
    let genesis = if genesis_path.exists() {
        GenesisConfig::load(&genesis_path)?
    } else {
        GenesisConfig::devnet()
    };

    if !args.mine && !args.rpc && args.listen.is_none() {
        println!(
            "tao-node {} — network '{}'. Pass --mine and/or --rpc / --listen ADDR.",
            tao_core::VERSION,
            config.network
        );
        return Ok(());
    }

    // Miner reward address (only required when this node produces blocks).
    let miner_pubkey = if args.mine {
        let s = args
            .miner
            .or_else(|| config.miner.reward_address.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("--mine requires --miner <PUBKEY> or miner.reward_address")
            })?;
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

    // The consensus PoW algorithm is a **genesis rule** (committed into the
    // genesis id via the config hash): every node on a network derives the same
    // PoW stack from the same genesis file. CLI flags override it for local
    // experiments only — an overridden node forks off any network running the
    // genesis rules, so warn loudly.
    let default_n = 8usize;
    let default_rank = 2usize;
    let use_matmul = args.matmul || args.matmul_n.is_some() || args.matmul_rank.is_some();
    let n = args.matmul_n.unwrap_or(default_n);
    let rank = args.matmul_rank.unwrap_or(default_rank);

    let cli_override = args.pouw || use_matmul || args.pow_switch_height.is_some();
    let (algorithm, switch_height) = if cli_override {
        tracing::warn!(
            "PoW overridden by CLI flags — this node will NOT follow the genesis \
             consensus rules; use only on throwaway local networks"
        );
        let alg = if args.pouw { "pouw" } else { "matmul" };
        (alg.to_string(), args.pow_switch_height)
    } else {
        (genesis.pow.algorithm.clone(), genesis.pow.switch_height)
    };

    let base_pow: Arc<dyn tao_consensus::PowAlgorithm> = match algorithm.as_str() {
        "pouw" => {
            // Utility-gated matmul-PoUW: the block PoW is the *genesis-committed*
            // model's layer applied to a per-block input (real model computation,
            // not free matrices). The model is derived deterministically from the
            // genesis weight seed, so every node agrees on the model and its id.
            let mp = genesis.pouw.as_ref().ok_or_else(|| {
                anyhow::anyhow!("pow algorithm 'pouw' requires a [pouw] model committed in genesis")
            })?;
            let seed = tao_consensus::genesis::parse_target(&mp.weight_seed)
                .map_err(|e| anyhow::anyhow!("bad pouw weight_seed: {e}"))?;
            let gate = tao_pouw::UtilityGatePow::from_seed(&mp.name, mp.n, mp.rank, mp.tiles, seed);
            let derived = hex_bytes(&gate.model_id());
            if let Some(expected) = &mp.model_id {
                if &derived != expected {
                    return Err(anyhow::anyhow!(
                        "pouw model id mismatch: genesis pins {expected} but derived {derived}"
                    ));
                }
            }
            tracing::info!(
                model = %mp.name, model_id = %derived, n = mp.n, rank = mp.rank, tiles = mp.tiles,
                "utility-gated matmul-PoUW consensus (genesis-committed model)"
            );
            Arc::new(gate)
        }
        // Plain matmul-PoUW (matrix multiplication as the PoW work). Size comes
        // from CLI flags (genesis-committed sizing rides on the pouw model).
        "matmul" => Arc::new(tao_pouw::MatmulPow::new(n, rank)),
        "blake3" => Arc::new(Blake3Pow),
        other => return Err(anyhow::anyhow!("unknown pow algorithm '{other}' in genesis")),
    };
    let pow: Arc<dyn tao_consensus::PowAlgorithm> = match switch_height {
        // Blake3 (fair launch / CPU) until the committed switch height, then the
        // genesis algorithm (the M7 evolution story).
        Some(h) if algorithm != "blake3" => {
            Arc::new(tao_consensus::HeightSwitchPow::new(Arc::new(Blake3Pow), base_pow, h))
        }
        _ => base_pow,
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
        let listen_addr: SocketAddr = listen_str
            .parse()
            .map_err(|e| anyhow::anyhow!("bad --listen address: {e}"))?;
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
