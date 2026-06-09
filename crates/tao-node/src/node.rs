//! Node assembly and the M1 single-node mining loop.
//!
//! Wires the genesis config, the consensus [`ChainState`], and the durable
//! [`BlockLog`] together, replays any existing log on startup, and (optionally)
//! runs a CPU mining loop that produces empty blocks while the LWMA difficulty
//! converges toward the target block time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use primitive_types::U256;
use solana_pubkey::Pubkey;
use tao_consensus::{
    block::Block, genesis::genesis_header, grind, mine::GrindResult, Blake3Pow, BlockStatus,
    ChainState, DifficultyParams,
};
use tao_core::genesis::GenesisConfig;

/// How many nonces to try between timestamp refreshes / shutdown checks.
const GRIND_BATCH: u64 = 200_000;

/// Settings for a mining run.
pub struct MineOptions {
    pub data_dir: PathBuf,
    pub genesis: GenesisConfig,
    pub miner: Pubkey,
    /// Number of blocks to mine before returning. `0` = until interrupted.
    pub blocks: u64,
}

/// Load (or initialize) chain state from the block log under `data_dir`.
fn load_chain(
    data_dir: &PathBuf,
    genesis: &GenesisConfig,
) -> anyhow::Result<(ChainState, tao_database::BlockLog)> {
    let g = genesis_header(genesis).map_err(|e| anyhow!("genesis: {e}"))?;
    let params = DifficultyParams::new(genesis.pow.target_block_time_secs, genesis.pow.lwma_window);
    let mut chain = ChainState::new(g, params, Arc::new(Blake3Pow));

    let log = tao_database::BlockLog::open(data_dir.join("blocks.log"))
        .context("opening block log")?;

    let records = log.read_all().context("reading block log")?;
    let replayed = records.len();
    for bytes in records {
        let block: Block = bincode::deserialize(&bytes)
            .map_err(|e| anyhow!("corrupt block in log: {e}"))?;
        // Ignore non-extending results during replay; fork choice is deterministic.
        let _ = chain.add_header(block.header);
    }
    if replayed > 0 {
        tracing::info!(replayed, height = chain.height(), "replayed block log");
    }
    Ok((chain, log))
}

/// Count leading zero *bits* of a 32-byte target (a readable difficulty proxy).
fn leading_zero_bits(bytes: &[u8; 32]) -> u32 {
    let mut bits = 0;
    for b in bytes {
        if *b == 0 {
            bits += 8;
        } else {
            bits += b.leading_zeros();
            break;
        }
    }
    bits
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run the mining loop until `blocks` are produced or `shutdown` is set.
pub fn run_mining(opts: MineOptions, shutdown: Arc<AtomicBool>) -> anyhow::Result<()> {
    let (mut chain, log) = load_chain(&opts.data_dir, &opts.genesis)?;
    let pow = Blake3Pow;

    tracing::info!(
        network = %opts.genesis.network,
        miner = %opts.miner,
        start_height = chain.height(),
        target_block_time = opts.genesis.pow.target_block_time_secs,
        "starting CPU miner (blake3, M1)"
    );

    let mut produced = 0u64;
    let mut last_block_time = unix_now();

    while !shutdown.load(Ordering::Relaxed) {
        if opts.blocks != 0 && produced >= opts.blocks {
            break;
        }

        // Build a fresh empty candidate on the current tip.
        let mut header = chain.build_candidate(opts.miner, unix_now(), &[]);
        let grind_start = Instant::now();
        let mut total_hashes = 0u64;

        let solved = loop {
            match grind(&mut header, &pow, GRIND_BATCH) {
                GrindResult::Found { hashes } => {
                    total_hashes += hashes;
                    break true;
                }
                GrindResult::Exhausted { hashes } => {
                    total_hashes += hashes;
                    if shutdown.load(Ordering::Relaxed) {
                        break false;
                    }
                    // Refresh timestamp so the chain keeps advancing wall-clock.
                    header.timestamp = unix_now();
                }
            }
        };
        if !solved {
            break;
        }

        // Persist the full block, then apply it to chain state.
        let block = Block::new(header.clone(), Vec::new());
        let bytes = bincode::serialize(&block).expect("block serialization is infallible");
        log.append(&bytes).context("appending block to log")?;

        let status = chain
            .add_header(header.clone())
            .map_err(|e| anyhow!("self-mined block rejected: {e}"))?;

        let now = unix_now();
        let solvetime = now - last_block_time;
        last_block_time = now;
        produced += 1;

        let elapsed = grind_start.elapsed().as_secs_f64().max(1e-9);
        let hps = total_hashes as f64 / elapsed;
        let work = chain.tip_work();
        tracing::info!(
            height = header.height,
            solvetime_s = solvetime,
            zero_bits = leading_zero_bits(&header.target),
            hashes = total_hashes,
            hashrate_hs = format_args!("{hps:.0}"),
            cumulative_work = %work_human(work),
            status = ?status_label(&status),
            "mined block"
        );
    }

    tracing::info!(height = chain.height(), produced, "miner stopped");
    Ok(())
}

fn status_label(s: &BlockStatus) -> &'static str {
    match s {
        BlockStatus::ExtendedTip => "extend",
        BlockStatus::Reorg { .. } => "reorg",
        BlockStatus::SideChain => "side",
    }
}

/// Render a U256 work value compactly (decimal up to u128, else hex).
fn work_human(work: U256) -> String {
    if work <= U256::from(u128::MAX) {
        work.as_u128().to_string()
    } else {
        format!("0x{work:x}")
    }
}
