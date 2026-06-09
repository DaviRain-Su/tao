//! `DagChain` — a single-node blockDAG consensus core: multi-parent PoW blocks,
//! durable persistence, replay, and **correct merge handling**.
//!
//! State is a *derived cache*: it is (re)computed by executing the GHOSTDAG
//! total order through the SVM from genesis. This is O(history) per query but
//! always correct under parallel blocks / reorgs (the linear node uses the same
//! "rebuild from the log" approach). Incremental virtual-state processing is the
//! production optimization.
//!
//! Difficulty adjusts per block with an **LWMA** over the GHOSTDAG selected
//! chain (the heaviest chain of selected parents). This reuses the same,
//! already-tested [`tao_consensus::next_target`] LWMA the linear chain uses,
//! sampling the selected-parent chain by ascending blue score. [`DagChain::open`]
//! keeps difficulty fixed (window = `u64::MAX` ⇒ permanent warmup); use
//! [`DagChain::open_with_daa`] to enable adjustment.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use solana_account::AccountSharedData;
use solana_hash::Hash as Blockhash;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use tao_consensus::{
    meets_target, next_target, tx_merkle_root, BlockHeader as ConsHeader, DagBlock, DagBlockHeader,
    DifficultyParams, Target, HEADER_VERSION,
};
use tao_database::{AccountsDb, BlockLog};
use tao_ghostdag::{blockhash, DagEngine, Hash as DagHash};
use tao_runtime::{Bank, BankError};

/// Fixed env blockhash for SVM execution (does not affect state for plain transfers).
fn env_blockhash() -> Blockhash {
    Blockhash::new_from_array([7u8; 32])
}

/// A single-node blockDAG with PoW, persistence, and SVM-executed state.
pub struct DagChain {
    engine: DagEngine,
    log: BlockLog,
    tips: Vec<[u8; 32]>,
    blocks: HashSet<[u8; 32]>,
    block_txs: HashMap<[u8; 32], Vec<Transaction>>,
    /// Headers of every accepted block (incl. genesis), for DAA chain sampling.
    headers: HashMap<[u8; 32], DagBlockHeader>,
    genesis_dag: DagHash,
    genesis_target: Target,
    /// Desired seconds per block (LWMA `T`).
    block_time_secs: u64,
    /// LWMA window in blocks (`u64::MAX` ⇒ difficulty held fixed).
    lwma_window: u64,
    miner: Pubkey,
    reward: u64,
    accounts_dir: PathBuf,
    allocations: Vec<(Pubkey, u64)>,
    /// Cached "virtual" state and the exact block order it reflects, for
    /// incremental replay (append-only fast path; reorg ⇒ rebuild from the
    /// finalized checkpoint, or genesis if none).
    state: Option<Bank>,
    state_dir: PathBuf,
    executed: Vec<[u8; 32]>,
    /// Finalized state snapshot: a reorg rebuilds from here instead of genesis,
    /// bounding the work to the post-checkpoint suffix. `0` disables it.
    finality_depth: u64,
    checkpoint: Option<Checkpoint>,
    checkpoint_dir: PathBuf,
    /// Once finalized transaction bodies are pruned, state can only be rebuilt
    /// from the checkpoint (not genesis).
    history_pruned: bool,
}

/// A finalized state snapshot: the account set after executing the first `len`
/// blocks of the total order (whose last block is `last_id`).
struct Checkpoint {
    len: usize,
    last_id: [u8; 32],
    accounts: Vec<(Pubkey, AccountSharedData)>,
}

/// Blocks kept beyond the checkpoint before it advances (finality margin).
const DEFAULT_FINALITY_DEPTH: u64 = 100;

impl DagChain {
    /// Open (or create) a DAG chain under `data_dir` with **fixed** difficulty.
    /// The block log is replayed.
    pub fn open(
        data_dir: PathBuf,
        k: u16,
        target: Target,
        miner: Pubkey,
        reward: u64,
        allocations: Vec<(Pubkey, u64)>,
    ) -> Result<Self, String> {
        // window = u64::MAX ⇒ next_target is always in "warmup" ⇒ fixed target.
        Self::open_with_daa(data_dir, k, target, miner, reward, allocations, 10, u64::MAX)
    }

    /// Open (or create) a DAG chain with LWMA difficulty adjustment over the
    /// selected chain: `block_time_secs` is the desired seconds per block and
    /// `lwma_window` is the averaging window (in blocks).
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_daa(
        data_dir: PathBuf,
        k: u16,
        genesis_target: Target,
        miner: Pubkey,
        reward: u64,
        allocations: Vec<(Pubkey, u64)>,
        block_time_secs: u64,
        lwma_window: u64,
    ) -> Result<Self, String> {
        // Deterministic genesis header (no parents, no txs).
        let genesis_header = DagBlockHeader {
            version: HEADER_VERSION,
            parents: vec![],
            timestamp: 1_750_000_000,
            tx_merkle_root: Blockhash::default(),
            state_root: Blockhash::default(),
            target: genesis_target,
            nonce: 0,
            miner: Pubkey::default(),
        };
        let genesis_id = genesis_header.id();
        let genesis_dag = DagHash::from_bytes(genesis_id);

        let engine = DagEngine::new(k, genesis_dag);
        engine.add_block(genesis_dag, &[blockhash::ORIGIN]);

        let log = BlockLog::open(data_dir.join("dag.log")).map_err(|e| e.to_string())?;

        let mut chain = Self {
            engine,
            log,
            tips: vec![genesis_id],
            blocks: HashSet::from([genesis_id]),
            block_txs: HashMap::new(),
            headers: HashMap::from([(genesis_id, genesis_header)]),
            genesis_dag,
            genesis_target,
            block_time_secs,
            lwma_window,
            miner,
            reward,
            accounts_dir: data_dir.join("accounts"),
            allocations,
            state: None,
            state_dir: data_dir.join("state"),
            executed: Vec::new(),
            finality_depth: DEFAULT_FINALITY_DEPTH,
            checkpoint: None,
            checkpoint_dir: data_dir.join("checkpoint"),
            history_pruned: false,
        };

        // Replay the log.
        let records = chain.log.read_all().map_err(|e| e.to_string())?;
        for bytes in records {
            let block: DagBlock = bincode::deserialize(&bytes).map_err(|e| e.to_string())?;
            chain.apply(&block)?;
        }
        Ok(chain)
    }

    /// The target for the next block: an LWMA over the GHOSTDAG selected chain
    /// (ascending blue score). During warmup (or fixed mode) this returns the
    /// genesis / current target. Reuses [`tao_consensus::next_target`].
    pub fn next_target(&self) -> Target {
        // The next block's selected parent is the current heaviest tip.
        self.selected_chain_target(self.engine.tip())
    }

    /// The DAA-expected target for a *new* block referencing `parents`: derived
    /// from the LWMA over the selected parent's chain. The selected parent is the
    /// parent with the greatest blue work (ties by larger hash) — exactly
    /// GHOSTDAG's `find_selected_parent`, so every node computes the same target
    /// and a miner cannot lowball difficulty by referencing a weaker parent.
    pub fn expected_target(&self, parents: &[[u8; 32]]) -> Target {
        let sp = parents
            .iter()
            .copied()
            .map(DagHash::from_bytes)
            .map(|p| (self.engine.data(p).blue_work, p))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, p)| p)
            .unwrap_or(self.genesis_dag);
        self.selected_chain_target(sp)
    }

    /// LWMA target over the selected-parent chain ending at `selected_parent`.
    fn selected_chain_target(&self, selected_parent: DagHash) -> Target {
        let take = (self.lwma_window as usize).saturating_add(1);
        let mut cur = selected_parent;
        let mut chain: Vec<DagHash> = Vec::new();
        loop {
            chain.push(cur);
            if chain.len() >= take || cur == self.genesis_dag {
                break;
            }
            cur = self.engine.selected_parent(cur);
        }
        chain.reverse(); // ascending blue score (genesis-most first)

        let headers: Vec<ConsHeader> = chain
            .iter()
            .filter_map(|h| self.headers.get(&h.to_bytes()))
            .map(|h| ConsHeader {
                version: h.version,
                prev_hash: Blockhash::default(),
                height: 0,
                timestamp: h.timestamp,
                tx_merkle_root: Blockhash::default(),
                state_root: Blockhash::default(),
                target: h.target,
                nonce: 0,
                miner: Pubkey::default(),
            })
            .collect();

        let params = DifficultyParams::new(self.block_time_secs.max(1), self.lwma_window.max(2));
        next_target(&headers, &params, &self.genesis_target)
    }

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Build a PoW block referencing the current tips with the given transactions.
    pub fn build_block(&self, transactions: &[Transaction]) -> DagBlock {
        self.build_block_at(transactions, Self::now())
    }

    /// Like [`Self::build_block`] but with an explicit header timestamp (used to
    /// test the DAA deterministically without relying on wall-clock spacing).
    pub fn build_block_at(&self, transactions: &[Transaction], timestamp: i64) -> DagBlock {
        let serialized: Vec<Vec<u8>> =
            transactions.iter().map(|t| bincode::serialize(t).expect("tx serialize")).collect();
        let target = self.next_target();
        let mut header = DagBlockHeader {
            version: HEADER_VERSION,
            parents: self.tips.clone(),
            timestamp,
            tx_merkle_root: tx_merkle_root(&serialized),
            state_root: Blockhash::default(),
            target,
            nonce: 0,
            miner: self.miner,
        };
        while !meets_target(&header.id(), &target) {
            header.nonce = header.nonce.wrapping_add(1);
        }
        DagBlock { header, transactions: serialized }
    }

    /// Mine a block (build + persist + apply).
    pub fn mine(&mut self, transactions: &[Transaction]) -> Result<DagBlock, String> {
        self.mine_at(transactions, Self::now())
    }

    /// Mine a block with an explicit header timestamp.
    pub fn mine_at(&mut self, transactions: &[Transaction], timestamp: i64) -> Result<DagBlock, String> {
        let block = self.build_block_at(transactions, timestamp);
        self.log.append(&bincode::serialize(&block).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        self.apply(&block)?;
        Ok(block)
    }

    /// Accept an externally-produced block (e.g. from a peer): validate PoW,
    /// persist, and apply.
    pub fn accept(&mut self, block: DagBlock) -> Result<(), String> {
        if self.blocks.contains(&block.id()) {
            return Ok(()); // already have it (gossip duplicate)
        }
        if !meets_target(&block.header.id(), &block.header.target) {
            return Err("invalid proof-of-work".into());
        }
        if !block.header.parents.iter().all(|p| self.blocks.contains(p)) {
            return Err("unknown parent (orphan)".into());
        }
        // Consensus-enforced difficulty: the block must declare exactly the
        // DAA-expected target for its parents (else PoW could be lowballed).
        let expected = self.expected_target(&block.header.parents);
        if block.header.target != expected {
            return Err("unexpected difficulty target".into());
        }
        self.log.append(&bincode::serialize(&block).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        self.apply(&block)
    }

    /// Add a block to the in-memory DAG + GHOSTDAG (no persistence).
    fn apply(&mut self, block: &DagBlock) -> Result<(), String> {
        let id = block.id();
        if self.blocks.contains(&id) {
            return Ok(());
        }
        let parents: Vec<DagHash> = block.header.parents.iter().map(|p| DagHash::from_bytes(*p)).collect();
        let parents = if parents.is_empty() { vec![self.genesis_dag] } else { parents };
        self.engine.add_block(DagHash::from_bytes(id), &parents);

        let txs: Vec<Transaction> = block
            .transactions
            .iter()
            .map(|b| bincode::deserialize(b).map_err(|e| format!("decode tx: {e}")))
            .collect::<Result<_, String>>()?;
        if !txs.is_empty() {
            self.block_txs.insert(id, txs);
        }
        self.headers.insert(id, block.header.clone());
        self.blocks.insert(id);

        // Tips: drop the referenced parents, add the new block.
        self.tips.retain(|t| !block.header.parents.contains(t));
        self.tips.push(id);
        Ok(())
    }

    /// The GHOSTDAG total order (block ids), genesis first.
    pub fn total_order(&self) -> Vec<[u8; 32]> {
        self.engine.total_order().into_iter().map(|h| h.to_bytes()).collect()
    }

    /// Set the finality depth (blocks kept beyond the checkpoint). `0` disables
    /// checkpointing (reorgs then always rebuild from genesis).
    pub fn set_finality_depth(&mut self, depth: u64) {
        self.finality_depth = depth;
    }

    /// A fresh `Bank` over a wiped account store at `dir`. When `seed_genesis`,
    /// it is seeded with the genesis allocations (state before any block); when
    /// false it starts empty (used for restoring a checkpoint snapshot).
    fn open_bank(&self, dir: &Path, seed_genesis: bool) -> Result<Bank, BankError> {
        let _ = std::fs::remove_dir_all(dir);
        let db = Arc::new(AccountsDb::open(dir).map_err(|e| BankError::Storage(e.to_string()))?);
        if seed_genesis {
            let system = solana_sdk_ids::system_program::id();
            for (pubkey, lamports) in &self.allocations {
                db.set(pubkey, &AccountSharedData::new(*lamports, 0, &system))
                    .map_err(|e| BankError::Storage(e.to_string()))?;
            }
        }
        Ok(Bank::new(db, 0))
    }

    fn fresh_bank(&self, dir: &Path) -> Result<Bank, BankError> {
        self.open_bank(dir, true)
    }

    /// Execute the given block ids (in order) through `bank`: each non-genesis
    /// block credits the coinbase then runs its transactions.
    fn replay_blocks(&self, bank: &Bank, blocks: &[[u8; 32]]) -> Result<(), BankError> {
        let genesis = self.genesis_dag.to_bytes();
        let bh = env_blockhash();
        for block in blocks {
            if *block == genesis {
                continue;
            }
            if self.reward > 0 {
                bank.airdrop(&self.miner, self.reward)?;
            }
            if let Some(txs) = self.block_txs.get(block) {
                for tx in txs {
                    let _ = bank.execute_transaction(tx, bh.clone())?;
                }
            }
        }
        Ok(())
    }

    /// Rebuild the SVM state by executing the GHOSTDAG total order from genesis,
    /// returning a `Bank` over a freshly-built account store. Authoritative and
    /// always correct (used by [`Self::virtual_state`] as its reorg fallback and
    /// as the cross-check in tests).
    pub fn rebuild_state(&self) -> Result<Bank, BankError> {
        if self.history_pruned {
            return Err(BankError::Storage(
                "history pruned; query state via virtual_state".into(),
            ));
        }
        let bank = self.fresh_bank(&self.accounts_dir)?;
        self.replay_blocks(&bank, &self.total_order())?;
        Ok(bank)
    }

    /// The current "virtual" state, maintained incrementally: if the new total
    /// order extends the order we last executed, only the appended suffix is
    /// replayed onto the cached `Bank` (O(new blocks)); if the order reorged
    /// (a prefix changed — e.g. a merge reordered finalized-but-recent blocks),
    /// it falls back to a full rebuild. Result is always identical to
    /// [`Self::rebuild_state`].
    pub fn virtual_state(&mut self) -> Result<&Bank, BankError> {
        let order = self.total_order();
        let can_extend = self.state.is_some()
            && order.len() >= self.executed.len()
            && self.executed.as_slice() == &order[..self.executed.len()];

        if can_extend {
            let start = self.executed.len();
            let bank = self.state.take().expect("checked is_some");
            self.replay_blocks(&bank, &order[start..])?;
            self.state = Some(bank);
        } else {
            // Reorg (or first call): rebuild. Drop the cached Bank first so its
            // RocksDB releases state_dir before we wipe and recreate it.
            self.state = None;
            // If a finalized checkpoint still matches the new order, rebuild from
            // it (replay only the post-checkpoint suffix) instead of from genesis.
            let from_checkpoint = match &self.checkpoint {
                Some(cp) if order.len() >= cp.len && order[cp.len - 1] == cp.last_id => Some(cp.len),
                _ => None,
            };
            let bank = if let Some(cp_len) = from_checkpoint {
                let cp = self.checkpoint.as_ref().expect("matched above");
                let bank = self.open_bank(&self.state_dir, false)?;
                bank.accounts()
                    .commit(cp.accounts.iter().cloned())
                    .map_err(|e| BankError::Storage(e.to_string()))?;
                self.replay_blocks(&bank, &order[cp_len..])?;
                bank
            } else if self.history_pruned {
                // No matching checkpoint and history is gone — a reorg deeper
                // than the pruned/finalized point, which must never happen.
                return Err(BankError::Storage(
                    "reorg below pruned history (deeper than finality)".into(),
                ));
            } else {
                let bank = self.fresh_bank(&self.state_dir)?;
                self.replay_blocks(&bank, &order)?;
                bank
            };
            self.state = Some(bank);
        }
        self.executed = order;
        self.maybe_advance_checkpoint()?;
        Ok(self.state.as_ref().expect("set above"))
    }

    /// Advance the finalized checkpoint once enough blocks have accumulated
    /// beyond it. The new snapshot is computed from the previous checkpoint (or
    /// genesis) by replaying only the blocks between them — bounded work.
    fn maybe_advance_checkpoint(&mut self) -> Result<(), BankError> {
        let fd = self.finality_depth as usize;
        if fd == 0 {
            return Ok(());
        }
        let cp_len = self.checkpoint.as_ref().map(|c| c.len).unwrap_or(0);
        // Only advance in chunks (amortized), and never into the unfinalized tail.
        if self.executed.len() < cp_len + 2 * fd {
            return Ok(());
        }
        let target = self.executed.len() - fd;
        if target <= cp_len {
            return Ok(());
        }

        let bank = self.open_bank(&self.checkpoint_dir, self.checkpoint.is_none())?;
        if let Some(cp) = &self.checkpoint {
            bank.accounts()
                .commit(cp.accounts.iter().cloned())
                .map_err(|e| BankError::Storage(e.to_string()))?;
            self.replay_blocks(&bank, &self.executed[cp_len..target])?;
        } else {
            self.replay_blocks(&bank, &self.executed[..target])?;
        }
        let accounts = bank.accounts().dump().map_err(|e| BankError::Storage(e.to_string()))?;
        let last_id = self.executed[target - 1];
        self.checkpoint = Some(Checkpoint { len: target, last_id, accounts });
        Ok(())
    }

    /// Prune the transaction bodies of finalized blocks (those in the checkpoint
    /// prefix). Their effects are already captured in the checkpoint snapshot, so
    /// they are redundant for state; dropping them bounds the dominant memory
    /// cost. Returns how many blocks' transactions were dropped.
    ///
    /// This is consensus-safe: it touches no ordering, difficulty, or GHOSTDAG
    /// computation — only data already reflected in the snapshot. After it,
    /// state is queryable solely via [`Self::virtual_state`] (which rebuilds from
    /// the checkpoint); [`Self::rebuild_state`] (full replay from genesis) errors.
    ///
    /// NOTE: headers and the GHOSTDAG engine are retained (DAA samples the
    /// selected chain over them, and pruning them would change the difficulty
    /// window and risk diverging from non-pruned peers). Re-anchoring the engine
    /// below the checkpoint + compacting the durable log + serving sync from the
    /// pruning point are the heavier follow-ons.
    pub fn prune_finalized_transactions(&mut self) -> Result<usize, BankError> {
        let cp_len = match &self.checkpoint {
            Some(cp) => cp.len,
            None => return Ok(0),
        };
        let prefix: Vec<[u8; 32]> = self.executed[..cp_len].to_vec();
        let mut pruned = 0;
        for id in &prefix {
            if self.block_txs.remove(id).is_some() {
                pruned += 1;
            }
        }
        if pruned > 0 {
            self.history_pruned = true;
            // The cached state may have been built from genesis; force the next
            // query to rebuild from the checkpoint snapshot instead.
            self.state = None;
            self.executed.clear();
        }
        Ok(pruned)
    }

    /// Whether finalized transaction history has been pruned.
    pub fn is_history_pruned(&self) -> bool {
        self.history_pruned
    }

    /// Reconstruct a stored block by id (for serving peer backfill requests),
    /// or `None` if we don't have it. Genesis (no stored header txs) is included.
    pub fn get_block(&self, id: &[u8; 32]) -> Option<DagBlock> {
        let header = self.headers.get(id)?.clone();
        let transactions = self
            .block_txs
            .get(id)
            .map(|txs| txs.iter().map(|t| bincode::serialize(t).expect("tx serialize")).collect())
            .unwrap_or_default();
        Some(DagBlock { header, transactions })
    }

    /// True if this block id is known (accepted) by the chain.
    pub fn has_block(&self, id: &[u8; 32]) -> bool {
        self.blocks.contains(id)
    }

    pub fn tips(&self) -> &[[u8; 32]] {
        &self.tips
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;
    use solana_sdk_ids::system_program;
    use solana_signer::Signer;

    fn easy_target() -> Target {
        let mut t = [0xffu8; 32];
        t[0] = 0x00;
        t
    }

    fn dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tao-dagchain-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn transfer(
        from: &solana_keypair::Keypair,
        to: &Pubkey,
        lamports: u64,
    ) -> Transaction {
        let ix = solana_system_interface::instruction::transfer(&from.pubkey(), to, lamports);
        Transaction::new_signed_with_payer(&[ix], Some(&from.pubkey()), &[from], env_blockhash())
    }

    #[test]
    fn mine_persist_replay() {
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let d = dir("replay");

        let root_after_mining = {
            let mut chain = DagChain::open(
                d.clone(),
                3,
                easy_target(),
                miner,
                0,
                vec![(payer.pubkey(), 1_000_000_000)],
            )
            .unwrap();
            chain.mine(&[transfer(&payer, &a.pubkey(), 100_000_000)]).unwrap();
            chain.mine(&[transfer(&payer, &a.pubkey(), 50_000_000)]).unwrap();
            let bank = chain.rebuild_state().unwrap();
            assert_eq!(bank.balance(&a.pubkey()), 150_000_000);
            bank.state_root().unwrap()
        };

        // Reopen: replay the log, recompute state — must match.
        let chain = DagChain::open(
            d,
            3,
            easy_target(),
            miner,
            0,
            vec![(payer.pubkey(), 1_000_000_000)],
        )
        .unwrap();
        assert_eq!(chain.block_count(), 3); // genesis + 2
        let bank = chain.rebuild_state().unwrap();
        assert_eq!(bank.balance(&a.pubkey()), 150_000_000);
        assert_eq!(bank.state_root().unwrap(), root_after_mining, "replay reproduces state");
    }

    #[test]
    fn merge_of_parallel_blocks_linearizes_correctly() {
        // genesis; block1 (funds A); parallel block2 (funds B, built from genesis);
        // merge block3 (parents block1+block2) spends A→B. GHOSTDAG must order the
        // A-funding before the A→B spend across the merge.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let b = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();

        let run = |tag: &str| {
            let mut chain = DagChain::open(
                dir(tag),
                3,
                easy_target(),
                miner,
                0,
                vec![(payer.pubkey(), 1_000_000_000)],
            )
            .unwrap();
            // block1 off genesis: payer -> A
            chain.mine(&[transfer(&payer, &a.pubkey(), 100_000_000)]).unwrap();
            // block2 is a parallel block also off genesis (built as an "external" block):
            // temporarily point tips at genesis to build it, then accept it.
            let block2 = {
                let tmp = DagChain::open(
                    dir(&format!("{tag}-side")),
                    3,
                    easy_target(),
                    miner,
                    0,
                    vec![(payer.pubkey(), 1_000_000_000)],
                )
                .unwrap();
                tmp.build_block(&[transfer(&payer, &b.pubkey(), 50_000_000)])
            };
            chain.accept(block2).unwrap();
            // merge block3: references both current tips (block1 + block2), spends A->B
            chain.mine(&[transfer(&a, &b.pubkey(), 10_000_000)]).unwrap();

            let bank = chain.rebuild_state().unwrap();
            (bank.state_root().unwrap(), bank.balance(&a.pubkey()), bank.balance(&b.pubkey()))
        };

        let (root1, a_bal, b_bal) = run("merge1");
        assert_eq!(a_bal, 100_000_000 - 10_000_000 - 5_000, "A funded before spend across the merge");
        assert_eq!(b_bal, 60_000_000);
        let (root2, ..) = run("merge2");
        assert_eq!(root1, root2, "deterministic across runs");
        let _ = system_program::id();
    }

    #[test]
    fn two_miners_converge() {
        // Two independent miners produce blocks; after exchanging them, both
        // nodes must reach the SAME GHOSTDAG order and the SAME state — the
        // property that makes a multi-miner blockDAG sound.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let b = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let allocs = vec![(payer.pubkey(), 1_000_000_000)];

        let mut c1 = DagChain::open(dir("c1"), 3, easy_target(), miner, 0, allocs.clone()).unwrap();
        let mut c2 = DagChain::open(dir("c2"), 3, easy_target(), miner, 0, allocs.clone()).unwrap();

        // Miner 1 builds a two-block chain; miner 2 builds a parallel block.
        let b1 = c1.mine(&[transfer(&payer, &a.pubkey(), 100_000_000)]).unwrap();
        let b2 = c1.mine(&[transfer(&payer, &b.pubkey(), 50_000_000)]).unwrap();
        let b3 = c2.mine(&[transfer(&payer, &a.pubkey(), 10_000_000)]).unwrap();

        // Gossip: exchange each other's blocks (in dependency order).
        c2.accept(b1).unwrap();
        c2.accept(b2).unwrap();
        c1.accept(b3).unwrap();

        // Both nodes now hold the same block set → identical order and state.
        assert_eq!(c1.total_order(), c2.total_order(), "miners converge on one order");
        let r1 = c1.rebuild_state().unwrap().state_root().unwrap();
        let r2 = c2.rebuild_state().unwrap().state_root().unwrap();
        assert_eq!(r1, r2, "miners converge on one state");
    }

    #[test]
    fn daa_raises_difficulty_on_fast_blocks() {
        use tao_consensus::work_for_target;
        let miner = Pubkey::new_unique();
        // block_time = 10s, window = 5. Mine `window + 1` blocks 1s apart (fast)
        // — all mined during warmup at the easy genesis target, so each grind is
        // cheap — then the *next* target must be harder than genesis.
        let window = 5u64;
        let mut chain = DagChain::open_with_daa(
            dir("daa-fast"),
            3,
            easy_target(),
            miner,
            0,
            vec![],
            10,
            window,
        )
        .unwrap();
        // During warmup next_target == genesis target.
        assert_eq!(chain.next_target(), easy_target(), "warmup holds genesis target");
        let base = 2_000_000i64;
        for i in 0..=window {
            chain.mine_at(&[], base + (i as i64)).unwrap(); // 1s apart
        }
        // Now the selected chain has window+1 non-genesis blocks → past warmup.
        let nt = chain.next_target();
        assert!(
            work_for_target(&nt) > work_for_target(&easy_target()),
            "fast blocks raise difficulty (more work than genesis)"
        );
    }

    #[test]
    fn virtual_state_matches_full_replay() {
        // The incremental virtual state must equal the authoritative full replay
        // after every step — pure appends (fast path) and a parallel-block merge
        // (which can reorder the recent order ⇒ reorg fallback).
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let b = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let allocs = vec![(payer.pubkey(), 1_000_000_000)];
        // reward > 0 so coinbase is exercised on both paths.
        let mut chain =
            DagChain::open(dir("vstate"), 3, easy_target(), miner, 1_000_000, allocs.clone()).unwrap();

        let check = |chain: &mut DagChain| {
            let v = chain.virtual_state().unwrap().state_root().unwrap();
            let r = chain.rebuild_state().unwrap().state_root().unwrap();
            assert_eq!(v, r, "virtual state diverged from full replay");
        };

        chain.mine(&[transfer(&payer, &a.pubkey(), 100_000_000)]).unwrap();
        check(&mut chain); // first call: full rebuild
        chain.mine(&[transfer(&payer, &b.pubkey(), 50_000_000)]).unwrap();
        check(&mut chain); // append fast path

        // A parallel block off genesis, then a merge that spends from A.
        let block2 = {
            let tmp =
                DagChain::open(dir("vstate-side"), 3, easy_target(), miner, 1_000_000, allocs).unwrap();
            tmp.build_block(&[transfer(&payer, &a.pubkey(), 7_000_000)])
        };
        chain.accept(block2).unwrap();
        check(&mut chain); // may reorg the recent order ⇒ fallback
        chain.mine(&[transfer(&a, &b.pubkey(), 3_000_000)]).unwrap();
        check(&mut chain);
    }

    #[test]
    fn checkpoint_rebuild_matches_full_replay() {
        // With checkpointing on (small finality depth), build a chain, then cause
        // a reorg *after* the checkpoint (a sibling of a recent block). The reorg
        // rebuild must run from the checkpoint and still equal the authoritative
        // full replay from genesis.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let mut chain = DagChain::open(
            dir("ckpt"),
            3,
            easy_target(),
            miner,
            1_000_000,
            vec![(payer.pubkey(), 1_000_000_000)],
        )
        .unwrap();
        chain.set_finality_depth(2);

        let mut mined = Vec::new();
        for i in 0..12 {
            let txs = if i == 0 {
                vec![transfer(&payer, &a.pubkey(), 100_000_000)]
            } else {
                vec![]
            };
            mined.push(chain.mine(&txs).unwrap());
            chain.virtual_state().unwrap(); // advance executed + checkpoint
        }
        assert!(chain.checkpoint.is_some(), "a finalized checkpoint formed");
        let cp_len = chain.checkpoint.as_ref().unwrap().len;

        // Build a sibling of a recent (post-checkpoint) block → reorders the tail.
        let parent = mined[mined.len() - 3].id();
        let target = chain.expected_target(&[parent]);
        let mut h = DagBlockHeader {
            version: HEADER_VERSION,
            parents: vec![parent],
            timestamp: 9_000_000,
            tx_merkle_root: tx_merkle_root(&[]),
            state_root: Blockhash::default(),
            target,
            nonce: 0,
            miner,
        };
        while !meets_target(&h.id(), &target) {
            h.nonce = h.nonce.wrapping_add(1);
        }
        chain.accept(DagBlock { header: h, transactions: vec![] }).unwrap();

        // The checkpoint prefix is untouched, so the rebuild runs from it.
        assert_eq!(chain.checkpoint.as_ref().unwrap().len, cp_len, "checkpoint unchanged");
        let v = chain.virtual_state().unwrap().state_root().unwrap();
        let r = chain.rebuild_state().unwrap().state_root().unwrap();
        assert_eq!(v, r, "checkpoint-based rebuild equals full replay");
    }

    #[test]
    fn pruning_finalized_txs_preserves_state() {
        // Dropping the transaction bodies of finalized (pre-checkpoint) blocks
        // must not change the state: the checkpoint snapshot already reflects
        // them, so virtual_state rebuilds the same root from the snapshot.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let mut chain = DagChain::open(
            dir("prune"),
            3,
            easy_target(),
            miner,
            1_000_000,
            vec![(payer.pubkey(), 1_000_000_000)],
        )
        .unwrap();
        chain.set_finality_depth(2);
        for i in 0..12 {
            let txs = if i < 2 {
                vec![transfer(&payer, &a.pubkey(), 50_000_000)]
            } else {
                vec![]
            };
            chain.mine(&txs).unwrap();
            chain.virtual_state().unwrap();
        }
        assert!(chain.checkpoint.is_some());

        let before = chain.virtual_state().unwrap().state_root().unwrap();
        let txs_before = chain.block_txs.len();
        let pruned = chain.prune_finalized_transactions().unwrap();
        assert!(pruned > 0, "some finalized tx bodies were dropped");
        assert!(chain.block_txs.len() < txs_before, "block_txs shrank");
        assert!(chain.is_history_pruned());
        // Full replay from genesis is no longer possible.
        assert!(chain.rebuild_state().is_err());
        // But the snapshot-based virtual state still reproduces the same root.
        let after = chain.virtual_state().unwrap().state_root().unwrap();
        assert_eq!(before, after, "state preserved after pruning finalized tx bodies");
    }

    #[test]
    fn get_block_round_trips_for_backfill() {
        // A served block must reconstruct identically (same id) so a peer can
        // accept it during ancestor backfill.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let mut chain = DagChain::open(
            dir("getblock"),
            3,
            easy_target(),
            miner,
            0,
            vec![(payer.pubkey(), 1_000_000_000)],
        )
        .unwrap();
        let mined = chain.mine(&[transfer(&payer, &a.pubkey(), 100_000_000)]).unwrap();
        let served = chain.get_block(&mined.id()).expect("have the block");
        assert_eq!(served.id(), mined.id(), "reconstructed block id matches");
        assert_eq!(served.transactions, mined.transactions, "txs round-trip");
        assert!(chain.has_block(&mined.id()));
        assert!(chain.get_block(&[9u8; 32]).is_none(), "unknown id → None");
    }

    #[test]
    fn rejects_lowballed_difficulty() {
        // A block that declares an easier-than-expected target (to cheapen its
        // PoW) must be rejected even though its (easy) PoW "passes".
        let miner = Pubkey::new_unique();
        let mut chain = DagChain::open(dir("lowball"), 3, easy_target(), miner, 0, vec![]).unwrap();
        // Build a valid external block off genesis, then tamper its target.
        let side = DagChain::open(dir("lowball-side"), 3, easy_target(), miner, 0, vec![]).unwrap();
        let mut block = side.build_block(&[]);
        block.header.target = [0xff; 32]; // trivially-easy target
        while !meets_target(&block.header.id(), &block.header.target) {
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
        let err = chain.accept(block).unwrap_err();
        assert!(err.contains("unexpected difficulty"), "expected difficulty rejection, got: {err}");
        assert_eq!(chain.block_count(), 1, "the lowballed block was not accepted");
    }

    #[test]
    fn daa_holds_steady_on_time() {
        use tao_consensus::work_for_target;
        let miner = Pubkey::new_unique();
        // Blocks arriving exactly on time (10s apart) keep difficulty ~stable.
        let window = 5u64;
        let mut chain = DagChain::open_with_daa(
            dir("daa-steady"),
            3,
            easy_target(),
            miner,
            0,
            vec![],
            10,
            window,
        )
        .unwrap();
        let base = 3_000_000i64;
        for i in 0..=window {
            chain.mine_at(&[], base + (i as i64) * 10).unwrap(); // exactly on time
        }
        let nt = chain.next_target();
        let wg = work_for_target(&easy_target());
        let wn = work_for_target(&nt);
        // within ~5% of the genesis difficulty
        assert!(wn >= wg * primitive_types::U256::from(95u64) / primitive_types::U256::from(100u64));
        assert!(wn <= wg * primitive_types::U256::from(105u64) / primitive_types::U256::from(100u64));
    }
}
