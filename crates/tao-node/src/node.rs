//! Node assembly and the mining loop.
//!
//! Builds genesis + consensus [`ChainState`] + durable [`BlockLog`] + execution
//! [`Bank`] (embedded SVM over `AccountsDb`). The miner drains the mempool into
//! each block, executes it through the Bank, stamps `state_root` into the
//! header, mines PoW (pluggable: Blake3, MatmulPow / HeightSwitchPow for the
//! AI-shaped matrix PoW, etc.), and records signature statuses for the RPC.
//! On startup the block log is replayed and re-executed, verifying every block's
//! `state_root`. The PoW algorithm is supplied via [`MineOptions::pow`] so the
//! live miner and chain verification use the same instance (enabling M7 matmul-PoUW).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use tao_consensus::{
    block::Block, genesis::genesis_header, grind, mine::GrindResult, BlockStatus, ChainState,
    DifficultyParams, PowAlgorithm,
};
use tao_core::{genesis::GenesisConfig, Hash};
use tao_database::{AccountsDb, BlockLog};
use tao_p2p::{NetMsg, Network};
use tao_runtime::{load_allocations, Bank};

use crate::shared::Shared;

// Default batch size for cheap PoW (Blake3 etc.). For expensive matmul-PoUW we use a much
// smaller batch so we can refresh timestamp and difficulty more frequently.
const GRIND_BATCH_FAST: u64 = 200_000;
const GRIND_BATCH_SLOW: u64 = 200; // ~O(n^3) per attempt — keep responsive

/// Settings for a mining run.
pub struct MineOptions {
    pub data_dir: PathBuf,
    pub genesis: GenesisConfig,
    pub miner: Pubkey,
    /// Number of blocks to mine before returning. `0` = until interrupted.
    pub blocks: u64,
    /// Whether this node produces blocks (a follower sets this false).
    pub mine: bool,
    /// Faucet keypair secret (64 bytes) for `requestAirdrop`, if enabled.
    pub faucet: Option<[u8; 64]>,
    /// The PoW algorithm instance used for both chain verification (on replay/startup)
    /// and for mining new blocks. Pass e.g. `Arc::new(Blake3Pow)` (default) or
    /// `Arc::new(tao_pouw::MatmulPow::new(8, 2))` / `HeightSwitchPow` for the matrix AI PoW.
    pub pow: Arc<dyn PowAlgorithm>,
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
    tx.signatures
        .first()
        .and_then(|s| s.as_ref().try_into().ok())
}

/// A prepared miner: owns the consensus, log, and execution state.
pub struct Miner {
    chain: ChainState,
    log: BlockLog,
    bank: Bank,
    genesis: GenesisConfig,
    miner: Pubkey,
    blocks: u64,
    mine: bool,
    /// Active PoW algorithm (Blake3, MatmulPow, HeightSwitchPow, etc.).
    pow: Arc<dyn PowAlgorithm>,
}

/// Build chain + account state (replaying & re-executing the log) and the
/// shared state handed to the RPC server.
pub fn prepare(opts: MineOptions) -> anyhow::Result<(Miner, Arc<Shared>)> {
    let g = genesis_header(&opts.genesis).map_err(|e| anyhow!("genesis header: {e}"))?;
    let genesis_hash = g.id();
    let params = DifficultyParams::new(
        opts.genesis.pow.target_block_time_secs,
        opts.genesis.pow.lwma_window,
    );
    let mut chain = ChainState::new(g, params, opts.pow.clone());

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
        tracing::info!(
            replayed,
            height = chain.height(),
            "replayed and re-executed block log"
        );
    }

    let shared = Arc::new(Shared::new(
        accounts,
        genesis_hash,
        chain.height(),
        chain.tip_id(),
        opts.faucet,
    ));
    let miner = Miner {
        chain,
        log,
        bank,
        genesis: opts.genesis,
        miner: opts.miner,
        blocks: opts.blocks,
        mine: opts.mine,
        pow: opts.pow,
    };
    Ok((miner, shared))
}

impl Miner {
    /// Run the node loop until `blocks` are produced (miner) or `shutdown` is
    /// set. Applies inbound gossip (peer blocks + transactions) every iteration;
    /// a miner also produces a block each iteration, a follower idles.
    pub fn run(
        mut self,
        shared: Arc<Shared>,
        shutdown: Arc<AtomicBool>,
        network: Option<Network>,
        inbound: Option<Receiver<NetMsg>>,
    ) -> anyhow::Result<()> {
        let mode = if self.mine { "miner" } else { "follower" };
        tracing::info!(
            network = %self.genesis.network,
            mode,
            start_height = self.chain.height(),
            pow = self.pow.name(),
            "node starting (PoW mining + SVM execution)"
        );

        let mut produced = 0u64;
        let mut last_block_time = unix_now();

        while !shutdown.load(Ordering::Relaxed) {
            if self.mine && self.blocks != 0 && produced >= self.blocks {
                break;
            }

            // Apply inbound gossip.
            let mut did_work = false;
            if let Some(rx) = &inbound {
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        NetMsg::NewBlock(bytes) => {
                            self.apply_peer_block(&bytes, &shared)?;
                            did_work = true;
                        }
                        NetMsg::NewTx(bytes) => {
                            if let Ok(tx) = bincode::deserialize::<Transaction>(&bytes) {
                                shared.submit(tx);
                            }
                        }
                        // The linear chain doesn't serve DAG block/tip/snapshot sync.
                        NetMsg::GetBlock(_)
                        | NetMsg::GetTips
                        | NetMsg::Tips(_)
                        | NetMsg::GetSnapshot
                        | NetMsg::Snapshot(_) => {}
                    }
                }
            }

            if self.mine {
                self.mine_one(
                    &shared,
                    network.as_ref(),
                    &mut produced,
                    &mut last_block_time,
                )?;
            } else if !did_work {
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        tracing::info!(height = self.chain.height(), produced, "node stopped");
        Ok(())
    }

    /// Produce one block: drain mempool, execute, mine PoW, persist, broadcast.
    fn mine_one(
        &mut self,
        shared: &Arc<Shared>,
        network: Option<&Network>,
        produced: &mut u64,
        last_block_time: &mut i64,
    ) -> anyhow::Result<()> {
        let pow = self.pow.as_ref();
        let height = self.chain.height() + 1;
        let reward = emission(&self.genesis, height);
        let parent_hash = Hash::new_from_array(self.chain.tip_id());

        let txs = shared.drain_mempool();
        let serialized: Vec<Vec<u8>> = txs
            .iter()
            .map(|t| bincode::serialize(t).expect("tx serialize"))
            .collect();
        let exec = self
            .bank
            .execute_block(&txs, parent_hash, &self.miner, reward)?;

        let mut header = self
            .chain
            .build_candidate(self.miner, unix_now(), &serialized);
        header.state_root = Hash::new_from_array(exec.state_root);

        let grind_start = Instant::now();
        let batch = if pow.name().contains("matmul") {
            GRIND_BATCH_SLOW
        } else {
            GRIND_BATCH_FAST
        };
        let mut total_attempts = 0u64;
        loop {
            match grind(&mut header, pow, batch) {
                GrindResult::Found { hashes: attempts } => {
                    total_attempts += attempts;
                    break;
                }
                GrindResult::Exhausted { hashes: attempts } => {
                    total_attempts += attempts;
                    header.timestamp = unix_now();
                }
            }
        }

        let block = Block::new(header.clone(), serialized);
        let block_bytes = bincode::serialize(&block).expect("block serialize");
        self.log.append(&block_bytes).context("append block")?;
        let status = self
            .chain
            .add_header(header.clone())
            .map_err(|e| anyhow!("self-mined block rejected: {e}"))?;

        for (tx, outcome) in txs.iter().zip(exec.outcomes.iter()) {
            if let Some(sig) = signature_bytes(tx) {
                shared.confirm(sig, outcome.error.clone());
            }
        }
        shared.advance(height, self.chain.tip_id());
        if let Some(net) = network {
            net.broadcast(&NetMsg::NewBlock(block_bytes));
        }

        let now = unix_now();
        let solvetime = now - *last_block_time;
        *last_block_time = now;
        *produced += 1;

        let rate = total_attempts as f64 / grind_start.elapsed().as_secs_f64().max(1e-9);
        tracing::info!(
            height = header.height,
            txs = txs.len(),
            solvetime_s = solvetime,
            zero_bits = leading_zero_bits(&header.target),
            miner_balance = self.bank.balance(&self.miner),
            work_rate = format_args!("{rate:.0}"),
            peers = network.map(|n| n.peer_count()).unwrap_or(0),
            status = ?status_label(&status),
            "mined block"
        );
        Ok(())
    }

    /// Validate, execute, and apply a block received from a peer.
    fn apply_peer_block(&mut self, bytes: &[u8], shared: &Arc<Shared>) -> anyhow::Result<()> {
        let block: Block =
            bincode::deserialize(bytes).map_err(|e| anyhow!("decode peer block: {e}"))?;
        let id = block.id();
        if self.chain.contains(&id) {
            return Ok(());
        }
        let height = block.header.height;
        let parent_hash = block.header.prev_hash.clone();
        let block_miner = block.header.miner;
        let expected = block.header.state_root.to_bytes();
        let reward = emission(&self.genesis, height);
        let txs = decode_txs(&block.transactions)?;

        if let Err(e) = self.chain.add_header(block.header) {
            tracing::warn!(height, error = %e, "rejected peer block (orphan/invalid)");
            return Ok(());
        }
        let exec = self
            .bank
            .execute_block(&txs, parent_hash, &block_miner, reward)?;
        if exec.state_root != expected {
            tracing::error!(height, "peer block state-root mismatch");
            return Ok(());
        }
        self.log.append(bytes).context("append peer block")?;
        for (tx, outcome) in txs.iter().zip(exec.outcomes.iter()) {
            if let Some(sig) = signature_bytes(tx) {
                shared.confirm(sig, outcome.error.clone());
            }
        }
        shared.advance(height, self.chain.tip_id());
        tracing::info!(height, txs = txs.len(), "applied peer block");
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
