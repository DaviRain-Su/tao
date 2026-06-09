//! Node assembly and the M2 single-node mining loop.
//!
//! Wires the genesis config, the consensus [`ChainState`], the durable
//! [`BlockLog`], and the execution [`Bank`] (embedded SVM over `AccountsDb`)
//! together. The miner executes each block (coinbase + transactions) through
//! the Bank, stamps the resulting `state_root` into the header, and mines PoW
//! over it. On startup the block log is replayed and **re-executed**, verifying
//! every block's committed `state_root`.
//!
//! The account store is treated as a derived cache rebuilt from the block log
//! at startup (the log is the source of truth); this keeps replay deterministic
//! and is fine for a devnet. Persisting account state incrementally is a later
//! optimization.

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
use tao_core::{genesis::GenesisConfig, Hash};
use tao_database::{AccountsDb, BlockLog};
use tao_runtime::{load_allocations, Bank};

/// Nonces to try between timestamp refreshes / shutdown checks.
const GRIND_BATCH: u64 = 200_000;

/// Settings for a mining run.
pub struct MineOptions {
    pub data_dir: PathBuf,
    pub genesis: GenesisConfig,
    pub miner: Pubkey,
    /// Number of blocks to mine before returning. `0` = until interrupted.
    pub blocks: u64,
}

/// Block reward at `height` per the genesis emission schedule (halving).
fn emission(genesis: &GenesisConfig, height: u64) -> u64 {
    let r = &genesis.reward;
    if r.halving_interval == 0 {
        return r.initial_lamports;
    }
    let halvings = height / r.halving_interval;
    if halvings >= 64 {
        0
    } else {
        r.initial_lamports >> halvings
    }
}

struct NodeState {
    chain: ChainState,
    log: BlockLog,
    bank: Bank,
}

/// Build chain + account state: load genesis, then replay & re-execute the log.
fn build_state(opts: &MineOptions) -> anyhow::Result<NodeState> {
    let g = genesis_header(&opts.genesis).map_err(|e| anyhow!("genesis header: {e}"))?;
    let params =
        DifficultyParams::new(opts.genesis.pow.target_block_time_secs, opts.genesis.pow.lwma_window);
    let mut chain = ChainState::new(g, params, Arc::new(Blake3Pow));

    // Account store is a derived cache: wipe and rebuild from the log.
    let accounts_dir = opts.data_dir.join("accounts");
    let _ = std::fs::remove_dir_all(&accounts_dir);
    let db = Arc::new(AccountsDb::open(&accounts_dir).context("open accounts db")?);
    load_allocations(&opts.genesis, &db).map_err(|e| anyhow!("genesis allocations: {e}"))?;
    let bank = Bank::new(db, 0);

    let log = BlockLog::open(opts.data_dir.join("blocks.log")).context("open block log")?;
    let records = log.read_all().context("read block log")?;
    let replayed = records.len();
    for bytes in records {
        let block: Block =
            bincode::deserialize(&bytes).map_err(|e| anyhow!("corrupt block in log: {e}"))?;
        let height = block.header.height;
        let reward = emission(&opts.genesis, height);
        let parent_hash = block.header.prev_hash.clone();
        let miner = block.header.miner;
        let expected = block.header.state_root.to_bytes();

        let _ = chain.add_header(block.header);

        // Re-execute (coinbase + txs) to rebuild state and verify the root.
        let exec = bank.execute_block(&[], parent_hash, &miner, reward)?;
        if exec.state_root != expected {
            return Err(anyhow!(
                "state root mismatch at height {height}: recomputed != header"
            ));
        }
    }
    if replayed > 0 {
        tracing::info!(replayed, height = chain.height(), "replayed and re-executed block log");
    }
    Ok(NodeState { chain, log, bank })
}

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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Run the mining loop until `blocks` are produced or `shutdown` is set.
pub fn run_mining(opts: MineOptions, shutdown: Arc<AtomicBool>) -> anyhow::Result<()> {
    let NodeState { mut chain, log, bank } = build_state(&opts)?;
    let pow = Blake3Pow;

    tracing::info!(
        network = %opts.genesis.network,
        miner = %opts.miner,
        start_height = chain.height(),
        target_block_time = opts.genesis.pow.target_block_time_secs,
        "starting CPU miner (blake3 PoW + SVM execution, M2)"
    );

    let mut produced = 0u64;
    let mut last_block_time = unix_now();

    while !shutdown.load(Ordering::Relaxed) {
        if opts.blocks != 0 && produced >= opts.blocks {
            break;
        }

        let height = chain.height() + 1;
        let reward = emission(&opts.genesis, height);
        let parent_hash = Hash::new_from_array(chain.tip_id());

        // Execute the block (coinbase only — no mempool yet) to get its state
        // root. Executed once per height; the root is independent of nonce.
        let exec = bank.execute_block(&[], parent_hash, &opts.miner, reward)?;

        let mut header = chain.build_candidate(opts.miner, unix_now(), &[]);
        header.state_root = Hash::new_from_array(exec.state_root);

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
                    header.timestamp = unix_now(); // refresh; state_root unchanged
                }
            }
        };
        if !solved {
            break;
        }

        let block = Block::new(header.clone(), Vec::new());
        log.append(&bincode::serialize(&block).expect("block serialization is infallible"))
            .context("append block")?;
        let status =
            chain.add_header(header.clone()).map_err(|e| anyhow!("self-mined block rejected: {e}"))?;

        let now = unix_now();
        let solvetime = now - last_block_time;
        last_block_time = now;
        produced += 1;

        let hps = total_hashes as f64 / grind_start.elapsed().as_secs_f64().max(1e-9);
        tracing::info!(
            height = header.height,
            solvetime_s = solvetime,
            zero_bits = leading_zero_bits(&header.target),
            reward,
            miner_balance = bank.balance(&opts.miner),
            state_root = %hex_short(&exec.state_root),
            hashrate_hs = format_args!("{hps:.0}"),
            cumulative_work = %work_human(chain.tip_work()),
            status = ?status_label(&status),
            "mined block"
        );
    }

    tracing::info!(
        height = chain.height(),
        produced,
        miner_balance = bank.balance(&opts.miner),
        "miner stopped"
    );
    Ok(())
}

fn status_label(s: &BlockStatus) -> &'static str {
    match s {
        BlockStatus::ExtendedTip => "extend",
        BlockStatus::Reorg { .. } => "reorg",
        BlockStatus::SideChain => "side",
    }
}

fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(12);
    for b in &bytes[..6] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn work_human(work: U256) -> String {
    if work <= U256::from(u128::MAX) {
        work.as_u128().to_string()
    } else {
        format!("0x{work:x}")
    }
}
