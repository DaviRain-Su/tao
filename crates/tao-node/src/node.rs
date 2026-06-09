//! Node assembly and the M2–M4 mining loop.
//!
//! Builds genesis + consensus [`ChainState`] + durable [`BlockLog`] + execution
//! [`Bank`] (embedded SVM over `AccountsDb`). The miner drains the mempool into
//! each block, executes it through the Bank, stamps `state_root` into the
//! header, mines PoW, and records signature statuses for the RPC. On startup the
//! block log is replayed and re-executed, verifying every block's `state_root`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use primitive_types::U256;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use tao_consensus::{
    block::Block, genesis::genesis_header, grind, mine::GrindResult, Blake3Pow, BlockStatus,
    ChainState, DifficultyParams,
};
use tao_core::{genesis::GenesisConfig, Hash};
use tao_database::{AccountsDb, BlockLog};
use tao_runtime::{load_allocations, Bank};

use crate::shared::Shared;

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

fn decode_txs(serialized: &[Vec<u8>]) -> anyhow::Result<Vec<Transaction>> {
    serialized
        .iter()
        .map(|b| bincode::deserialize::<Transaction>(b).map_err(|e| anyhow!("decode tx: {e}")))
        .collect()
}

fn signature_bytes(tx: &Transaction) -> Option<[u8; 64]> {
    tx.signatures.first().and_then(|s| s.as_ref().try_into().ok())
}

/// A prepared miner: owns the consensus, log, and execution state.
pub struct Miner {
    chain: ChainState,
    log: BlockLog,
    bank: Bank,
    genesis: GenesisConfig,
    miner: Pubkey,
    blocks: u64,
}

/// Build chain + account state (replaying & re-executing the log) and the
/// shared state handed to the RPC server.
pub fn prepare(opts: MineOptions) -> anyhow::Result<(Miner, Arc<Shared>)> {
    let g = genesis_header(&opts.genesis).map_err(|e| anyhow!("genesis header: {e}"))?;
    let genesis_hash = g.id();
    let params =
        DifficultyParams::new(opts.genesis.pow.target_block_time_secs, opts.genesis.pow.lwma_window);
    let mut chain = ChainState::new(g, params, Arc::new(Blake3Pow));

    // Account store is a derived cache: wipe and rebuild from the log.
    let accounts_dir = opts.data_dir.join("accounts");
    let _ = std::fs::remove_dir_all(&accounts_dir);
    let accounts = Arc::new(AccountsDb::open(&accounts_dir).context("open accounts db")?);
    load_allocations(&opts.genesis, &accounts).map_err(|e| anyhow!("genesis allocations: {e}"))?;
    let bank = Bank::new(accounts.clone(), 0);

    let log = BlockLog::open(opts.data_dir.join("blocks.log")).context("open block log")?;
    let records = log.read_all().context("read block log")?;
    let replayed = records.len();
    for bytes in records {
        let block: Block =
            bincode::deserialize(&bytes).map_err(|e| anyhow!("corrupt block in log: {e}"))?;
        let height = block.header.height;
        let reward = emission(&opts.genesis, height);
        let parent_hash = block.header.prev_hash.clone();
        let block_miner = block.header.miner;
        let expected = block.header.state_root.to_bytes();
        let txs = decode_txs(&block.transactions)?;

        let _ = chain.add_header(block.header);
        let exec = bank.execute_block(&txs, parent_hash, &block_miner, reward)?;
        if exec.state_root != expected {
            return Err(anyhow!("state root mismatch at height {height}"));
        }
    }
    if replayed > 0 {
        tracing::info!(replayed, height = chain.height(), "replayed and re-executed block log");
    }

    let shared = Arc::new(Shared::new(
        accounts,
        genesis_hash,
        chain.height(),
        chain.tip_id(),
    ));
    let miner = Miner {
        chain,
        log,
        bank,
        genesis: opts.genesis,
        miner: opts.miner,
        blocks: opts.blocks,
    };
    Ok((miner, shared))
}

impl Miner {
    /// Run the mining loop until `blocks` are produced or `shutdown` is set.
    pub fn run(mut self, shared: Arc<Shared>, shutdown: Arc<AtomicBool>) -> anyhow::Result<()> {
        let pow = Blake3Pow;
        tracing::info!(
            network = %self.genesis.network,
            miner = %self.miner,
            start_height = self.chain.height(),
            "starting CPU miner (blake3 PoW + SVM execution)"
        );

        let mut produced = 0u64;
        let mut last_block_time = unix_now();

        while !shutdown.load(Ordering::Relaxed) {
            if self.blocks != 0 && produced >= self.blocks {
                break;
            }

            let height = self.chain.height() + 1;
            let reward = emission(&self.genesis, height);
            let parent_hash = Hash::new_from_array(self.chain.tip_id());

            // Pull pending transactions and execute the block (coinbase + txs).
            let txs = shared.drain_mempool();
            let serialized: Vec<Vec<u8>> =
                txs.iter().map(|t| bincode::serialize(t).expect("tx serialize")).collect();
            let exec = self.bank.execute_block(&txs, parent_hash, &self.miner, reward)?;

            let mut header = self.chain.build_candidate(self.miner, unix_now(), &serialized);
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
                        header.timestamp = unix_now();
                    }
                }
            };
            if !solved {
                break;
            }

            let block = Block::new(header.clone(), serialized);
            self.log
                .append(&bincode::serialize(&block).expect("block serialize"))
                .context("append block")?;
            let status = self
                .chain
                .add_header(header.clone())
                .map_err(|e| anyhow!("self-mined block rejected: {e}"))?;

            // Confirm signatures for the included transactions.
            for (tx, outcome) in txs.iter().zip(exec.outcomes.iter()) {
                if let Some(sig) = signature_bytes(tx) {
                    shared.confirm(sig, outcome.error.clone());
                }
            }
            shared.advance(height, self.chain.tip_id());

            let now = unix_now();
            let solvetime = now - last_block_time;
            last_block_time = now;
            produced += 1;

            let hps = total_hashes as f64 / grind_start.elapsed().as_secs_f64().max(1e-9);
            tracing::info!(
                height = header.height,
                txs = txs.len(),
                solvetime_s = solvetime,
                zero_bits = leading_zero_bits(&header.target),
                miner_balance = self.bank.balance(&self.miner),
                hashrate_hs = format_args!("{hps:.0}"),
                cumulative_work = %work_human(self.chain.tip_work()),
                status = ?status_label(&status),
                "mined block"
            );
        }

        tracing::info!(
            height = self.chain.height(),
            produced,
            miner_balance = self.bank.balance(&self.miner),
            "miner stopped"
        );
        Ok(())
    }
}

fn status_label(s: &BlockStatus) -> &'static str {
    match s {
        BlockStatus::ExtendedTip => "extend",
        BlockStatus::Reorg { .. } => "reorg",
        BlockStatus::SideChain => "side",
    }
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

fn work_human(work: U256) -> String {
    if work <= U256::from(u128::MAX) {
        work.as_u128().to_string()
    } else {
        format!("0x{work:x}")
    }
}
