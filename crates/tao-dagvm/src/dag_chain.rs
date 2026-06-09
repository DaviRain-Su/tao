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
    meets_target, next_target, pow_level, tx_merkle_root, BlockHeader as ConsHeader, DagBlock,
    DagBlockHeader, DifficultyParams, Target, HEADER_VERSION,
};
use tao_database::{AccountsDb, BlockLog};
use tao_ghostdag::{blockhash, DagEngine, Hash as DagHash};
use tao_runtime::{Bank, BankError};

/// Fixed env blockhash for SVM execution (does not affect state for plain transfers).
fn env_blockhash() -> Blockhash {
    Blockhash::new_from_array([7u8; 32])
}

/// A durable log record. The log holds an optional leading `Snapshot` (written
/// by a re-anchor prune) followed by `Block` records for the kept suffix /
/// subsequently mined blocks. Compaction (`prune`) rewrites it as
/// `[Snapshot, Block…]`, so at most one snapshot is present and it leads.
#[derive(serde::Serialize, serde::Deserialize)]
enum LogRecord {
    Block(DagBlock),
    Snapshot {
        origin_header: DagBlockHeader,
        accounts: Vec<(Pubkey, AccountSharedData)>,
        proof: Vec<DagBlockHeader>,
    },
}

/// Legacy M8-raw `DagBlockHeader` format (pre `state_root` + `interlink`).
#[derive(serde::Serialize, serde::Deserialize)]
struct LegacyDagBlockHeader {
    /// Header format version.
    version: u32,
    /// Parent block ids (the GHOSTDAG parents). Empty only for genesis.
    parents: Vec<[u8; 32]>,
    /// Block timestamp (unix seconds).
    timestamp: i64,
    /// Merkle root over the block's transactions.
    tx_merkle_root: Blockhash,
    /// PoW target threshold (big-endian). `pow_hash <= target` wins.
    target: Target,
    /// PoW solution nonce.
    nonce: u64,
    /// Address that receives this block's coinbase reward.
    miner: Pubkey,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LegacyDagBlock {
    header: LegacyDagBlockHeader,
    transactions: Vec<Vec<u8>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum LegacyLogRecord {
    /// Old payload variant before the state-root/interlink migration.
    Block(LegacyDagBlock),
}

fn legacy_block_id(header: &LegacyDagBlockHeader) -> Result<[u8; 32], String> {
    bincode::serialize(header)
        .map(|bytes| *blake3::hash(&bytes).as_bytes())
        .map_err(|e| e.to_string())
}

/// Bootstrap payload a pruned node ships to a fresh/behind peer: the pruning
/// point (origin header + its account-set snapshot) plus the kept suffix blocks.
///
/// The suffix is applied as *trusted* (no PoW/difficulty re-validation): a pruned
/// node has discarded the history needed to re-derive the difficulty window for
/// those finalized blocks. A trustless bootstrap needs PoW pruning proofs (Kaspa)
/// — a documented follow-on; for now the snapshot is trusted like a checkpoint.
#[derive(serde::Serialize, serde::Deserialize)]
struct SyncSnapshot {
    origin_header: DagBlockHeader,
    accounts: Vec<(Pubkey, AccountSharedData)>,
    suffix: Vec<DagBlock>,
    /// NiPoPoW proof that `origin` is on a real, most-work chain from genesis.
    proof: Vec<DagBlockHeader>,
}

/// Verify a NiPoPoW pruning proof: an interlink-connected header chain
/// `genesis → origin`, each header carrying valid PoW. Anchored at `genesis_id`
/// (a known constant) and ending at `origin`. Returns the accumulated work.
fn verify_proof(
    proof: &[DagBlockHeader],
    origin: [u8; 32],
    genesis_id: [u8; 32],
) -> Result<primitive_types::U256, String> {
    if proof.is_empty() {
        return Err("empty proof".into());
    }
    if proof[0].id() != genesis_id {
        return Err("proof not anchored at genesis".into());
    }
    if proof.last().unwrap().id() != origin {
        return Err("proof does not end at the claimed origin".into());
    }
    // The proof is genesis-first (topologically ordered). Each non-genesis header
    // must carry valid PoW and point back — via an interlink entry or a parent —
    // to a header already seen, so the whole set is connected to genesis. This
    // tolerates the multi-level structure (consecutive proof headers need not be
    // adjacent in the DAG). Work is the sum over the sampled headers.
    let mut present: HashSet<[u8; 32]> = HashSet::new();
    present.insert(proof[0].id());
    let mut work = tao_consensus::work_for_target(&proof[0].target);
    for h in proof.iter().skip(1) {
        if !meets_target(&h.id(), &h.target) {
            return Err("invalid proof-of-work in proof".into());
        }
        let connected = h
            .interlink
            .iter()
            .chain(h.parents.iter())
            .any(|p| present.contains(p));
        if !connected {
            return Err("proof not interlink-connected".into());
        }
        work = work.saturating_add(tao_consensus::work_for_target(&h.target));
        present.insert(h.id());
    }
    Ok(work)
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
    k: u16,
    miner: Pubkey,
    reward: u64,
    accounts_dir: PathBuf,
    allocations: Vec<(Pubkey, u64)>,
    /// After a re-anchor prune, the post-prune genesis state (full accounts),
    /// replacing `allocations` as the base for state rebuilds.
    base_accounts: Option<Vec<(Pubkey, AccountSharedData)>>,
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
    /// Retained NiPoPoW proof for the current origin (pruning point): a succinct
    /// interlink-connected header chain genesis→origin that certifies the origin's
    /// accumulated PoW. Built before pruning (while ancestors exist) and shipped
    /// to bootstrapping peers. Empty until the chain has pruned.
    proof: Vec<DagBlockHeader>,
    /// True only if this node bootstrapped from a peer's snapshot (vs pruning its
    /// own genesis-rooted chain). A bootstrapped node may switch to a peer proof
    /// with strictly more work; a self-pruned node never adopts a peer snapshot.
    bootstrapped: bool,
    /// Accumulated work of the proof this node bootstrapped from (most-work rule).
    bootstrap_work: primitive_types::U256,
    /// The original chain genesis id (never changes under re-anchor/bootstrap).
    /// Pruning proofs always anchor here, so a bootstrapped node can still verify
    /// a competing peer's proof.
    original_genesis: [u8; 32],
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
        Self::open_with_daa(
            data_dir,
            k,
            target,
            miner,
            reward,
            allocations,
            10,
            u64::MAX,
        )
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
            interlink: Vec::new(),
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
            k,
            miner,
            reward,
            accounts_dir: data_dir.join("accounts"),
            allocations,
            base_accounts: None,
            state: None,
            state_dir: data_dir.join("state"),
            executed: Vec::new(),
            finality_depth: DEFAULT_FINALITY_DEPTH,
            checkpoint: None,
            checkpoint_dir: data_dir.join("checkpoint"),
            history_pruned: false,
            proof: Vec::new(),
            bootstrapped: false,
            bootstrap_work: primitive_types::U256::zero(),
            original_genesis: genesis_id,
        };

        // Replay the log: a leading snapshot (from a prune) re-anchors the chain,
        // then block records are applied on top.
        let records = chain.log.read_all().map_err(|e| e.to_string())?;
        for bytes in records {
            match bincode::deserialize::<LogRecord>(&bytes).map_err(|e| e.to_string()) {
                Ok(record) => match record {
                    LogRecord::Snapshot {
                        origin_header,
                        accounts,
                        proof,
                    } => {
                        chain.load_snapshot(origin_header, accounts, proof);
                    }
                    LogRecord::Block(block) => chain.apply(&block)?,
                },
                Err(primary_err) => {
                    if let Ok(LegacyLogRecord::Block(block)) =
                        bincode::deserialize::<LegacyLogRecord>(&bytes)
                    {
                        let block_id = legacy_block_id(&block.header)?;
                        let header = DagBlockHeader {
                            version: block.header.version,
                            parents: block.header.parents,
                            timestamp: block.header.timestamp,
                            tx_merkle_root: block.header.tx_merkle_root,
                            state_root: Blockhash::default(),
                            target: block.header.target,
                            interlink: Vec::new(),
                            nonce: block.header.nonce,
                            miner: block.header.miner,
                        };
                        chain.apply_with_id(
                            DagBlock {
                                header,
                                transactions: block.transactions,
                            },
                            block_id,
                        )?;
                    } else {
                        return Err(primary_err);
                    }
                }
            }
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
        self.selected_chain_target(self.selected_parent_dag(parents))
    }

    /// The GHOSTDAG selected parent of a block referencing `parents`: the parent
    /// with greatest blue work (ties by larger hash) — matching the engine.
    fn selected_parent_dag(&self, parents: &[[u8; 32]]) -> DagHash {
        parents
            .iter()
            .copied()
            .map(DagHash::from_bytes)
            .map(|p| (self.engine.data(p).blue_work, p))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, p)| p)
            .unwrap_or(self.genesis_dag)
    }

    /// Build a succinct NiPoPoW proof for `p`: walk the highest-level interlink
    /// back-pointer at each step from `p` until genesis (empty interlink). Each
    /// jump is the longest available, so the proof is logarithmic-ish in the work
    /// yet always interlink-connected (consecutive headers are linked by the
    /// followed pointer) and anchored at genesis. A longer chain yields a longer
    /// walk (more high-level blocks ⇒ more certified work), which is what the
    /// most-work comparison relies on. Must run before `p`'s ancestors are pruned.
    fn build_proof_for(&self, p: [u8; 32]) -> Vec<DagBlockHeader> {
        let mut proof = Vec::new();
        let mut cur = p;
        let mut guard = 0usize;
        loop {
            let h = match self.headers.get(&cur) {
                Some(h) => h.clone(),
                None => break, // ancestor already pruned — proof can't reach genesis
            };
            let interlink = h.interlink.clone();
            proof.push(h);
            match interlink.last() {
                None => break, // genesis (empty interlink)
                Some(&next) if next == cur => break,
                Some(&next) => cur = next, // jump to the highest-level ancestor
            }
            guard += 1;
            if guard > 1_000_000 {
                break;
            }
        }
        proof.reverse(); // genesis-first
        proof
    }

    /// The NiPoPoW interlink a block built on selected parent `sp` must commit to:
    /// `sp`'s interlink with entries `0..=level(sp)` updated to point at `sp`.
    fn interlink_for_parent(&self, sp: [u8; 32]) -> Vec<[u8; 32]> {
        let sp_header = match self.headers.get(&sp) {
            Some(h) => h,
            None => return Vec::new(),
        };
        let sp_level = pow_level(&sp, &sp_header.target) as usize;
        let mut il = sp_header.interlink.clone();
        for k in 0..=sp_level {
            if k < il.len() {
                il[k] = sp;
            } else {
                il.push(sp);
            }
        }
        il
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
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Build a PoW block referencing the current tips with the given transactions.
    pub fn build_block(&self, transactions: &[Transaction]) -> DagBlock {
        self.build_block_at(transactions, Self::now())
    }

    /// Like [`Self::build_block`] but with an explicit header timestamp (used to
    /// test the DAA deterministically without relying on wall-clock spacing).
    pub fn build_block_at(&self, transactions: &[Transaction], timestamp: i64) -> DagBlock {
        let serialized: Vec<Vec<u8>> = transactions
            .iter()
            .map(|t| {
                bincode::serialize(t).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "serialize tx for block build failed; using empty tx encoding");
                    Vec::new()
                })
            })
            .collect();
        let target = self.next_target();
        // Selected parent of a block referencing all tips is the heaviest tip.
        let interlink = self.interlink_for_parent(self.engine.tip().to_bytes());
        let mut header = DagBlockHeader {
            version: HEADER_VERSION,
            parents: self.tips.clone(),
            timestamp,
            tx_merkle_root: tx_merkle_root(&serialized),
            state_root: Blockhash::default(),
            target,
            interlink,
            nonce: 0,
            miner: self.miner,
        };
        while !meets_target(&header.id(), &target) {
            header.nonce = header.nonce.wrapping_add(1);
        }
        DagBlock {
            header,
            transactions: serialized,
        }
    }

    /// Mine a block (build + persist + apply).
    pub fn mine(&mut self, transactions: &[Transaction]) -> Result<DagBlock, String> {
        self.mine_at(transactions, Self::now())
    }

    /// Mine a block with an explicit header timestamp.
    pub fn mine_at(
        &mut self,
        transactions: &[Transaction],
        timestamp: i64,
    ) -> Result<DagBlock, String> {
        let block = self.build_block_at(transactions, timestamp);
        let rec =
            bincode::serialize(&LogRecord::Block(block.clone())).map_err(|e| e.to_string())?;
        self.log.append(&rec).map_err(|e| e.to_string())?;
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
            let missing_pruned_parent = block.header.parents.iter().any(|p| {
                *p != self.genesis_dag.to_bytes() && self.history_pruned && !self.blocks.contains(p)
            });
            if missing_pruned_parent {
                return Err("parent pruned".into());
            }
            return Err("unknown parent (orphan)".into());
        }
        // Consensus-enforced difficulty: the block must declare exactly the
        // DAA-expected target for its parents (else PoW could be lowballed).
        let sp = self.selected_parent_dag(&block.header.parents);
        let expected = self.selected_chain_target(sp);
        if block.header.target != expected {
            return Err("unexpected difficulty target".into());
        }
        // The interlink must be correctly derived from the selected parent (it is
        // PoW-committed, so a valid one proves the miner built it honestly).
        if block.header.interlink != self.interlink_for_parent(sp.to_bytes()) {
            return Err("invalid interlink".into());
        }
        let rec =
            bincode::serialize(&LogRecord::Block(block.clone())).map_err(|e| e.to_string())?;
        self.log.append(&rec).map_err(|e| e.to_string())?;
        self.apply(&block)
    }

    /// Add a block to the in-memory DAG + GHOSTDAG (no persistence).
    fn apply_with_id(&mut self, block: DagBlock, id: [u8; 32]) -> Result<(), String> {
        if self.blocks.contains(&id) {
            return Ok(());
        }
        // Map parents to DagHashes, remapping any parent we don't have (a pruned
        // ancestor) to the current origin, and de-duplicating (preserving order).
        let genesis_bytes = self.genesis_dag.to_bytes();
        let mut seen = HashSet::new();
        let mut parents: Vec<DagHash> = Vec::with_capacity(block.header.parents.len());
        for p in &block.header.parents {
            let d = if *p == genesis_bytes || self.blocks.contains(p) {
                DagHash::from_bytes(*p)
            } else {
                self.genesis_dag
            };
            if seen.insert(d.to_bytes()) {
                parents.push(d);
            }
        }
        if parents.is_empty() {
            parents.push(self.genesis_dag);
        }
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

    /// Add a block using its intrinsic id (used when loading legacy log records).
    fn apply(&mut self, block: &DagBlock) -> Result<(), String> {
        self.apply_with_id(block.clone(), block.id())
    }

    /// The GHOSTDAG total order (block ids), genesis first.
    pub fn total_order(&self) -> Vec<[u8; 32]> {
        self.engine
            .total_order()
            .into_iter()
            .map(|h| h.to_bytes())
            .collect()
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
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| BankError::Storage(e.to_string()))?;
        }
        let db = Arc::new(AccountsDb::open(dir).map_err(|e| BankError::Storage(e.to_string()))?);
        if seed_genesis {
            if let Some(base) = &self.base_accounts {
                // Post-prune: the genesis state is the re-anchor snapshot.
                db.commit(base.iter().cloned())
                    .map_err(|e| BankError::Storage(e.to_string()))?;
            } else {
                let system = solana_sdk_ids::system_program::id();
                for (pubkey, lamports) in &self.allocations {
                    db.set(pubkey, &AccountSharedData::new(*lamports, 0, &system))
                        .map_err(|e| BankError::Storage(e.to_string()))?;
                }
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
        if self.history_pruned && self.base_accounts.is_none() {
            // Transaction bodies were dropped but there is no re-anchor base to
            // start from, so a full replay is impossible — use virtual_state.
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
                Some(cp) if order.len() >= cp.len && order[cp.len - 1] == cp.last_id => {
                    Some(cp.len)
                }
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
            } else if self.history_pruned && self.base_accounts.is_none() {
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
        let accounts = bank
            .accounts()
            .dump()
            .map_err(|e| BankError::Storage(e.to_string()))?;
        let last_id = self.executed[target - 1];
        self.checkpoint = Some(Checkpoint {
            len: target,
            last_id,
            accounts,
        });
        Ok(())
    }

    /// Re-anchor the in-memory chain onto a snapshot origin. Used on replay when
    /// the log begins with a `Snapshot` record (written by a prior prune).
    fn load_snapshot(
        &mut self,
        origin_header: DagBlockHeader,
        accounts: Vec<(Pubkey, AccountSharedData)>,
        proof: Vec<DagBlockHeader>,
    ) {
        let origin = origin_header.id();
        let origin_dag = DagHash::from_bytes(origin);
        let engine = DagEngine::new(self.k, origin_dag);
        engine.add_block(origin_dag, &[blockhash::ORIGIN]);
        self.engine = engine;
        self.genesis_dag = origin_dag;
        self.tips = vec![origin];
        self.blocks = HashSet::from([origin]);
        self.headers = HashMap::from([(origin, origin_header)]);
        self.block_txs.clear();
        self.base_accounts = Some(accounts);
        self.history_pruned = true;
        self.checkpoint = None;
        self.proof = proof;
        self.state = None;
        self.executed.clear();
    }

    /// Prune the transaction bodies of finalized blocks (those in the checkpoint
    /// prefix). Their effects are already captured in the checkpoint snapshot, so
    /// they are redundant for state; dropping them bounds the dominant memory
    /// cost. Returns how many blocks' transactions were dropped.
    ///
    /// This is consensus-safe: it touches no ordering, difficulty, or GHOSTDAG
    /// computation — only data already reflected in the snapshot.
    /// The effect is process-local and not durable by itself:
    /// it does not compact `dag.log`, so after restart the same checkpoint prefix
    /// (including transaction bodies) is replayed from disk.
    ///
    /// After a restart, this method’s memory reduction is therefore temporary unless
    /// followed by [`Self::prune`], which rewrites logs and headers for durable
    /// history drop.
    ///
    /// After `prune_finalized_transactions`, state is queryable solely via
    /// [`Self::virtual_state`] (which rebuilds from the checkpoint); [`Self::rebuild_state`]
    /// (full replay from genesis) returns an error.
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

    /// Export a bootstrap snapshot (pruning point + base accounts + kept suffix)
    /// for syncing a fresh/behind peer. `None` if this node hasn't pruned (such a
    /// node serves full history via block backfill instead).
    pub fn export_snapshot(&self) -> Option<Vec<u8>> {
        if !self.history_pruned {
            return None;
        }
        let origin = self.genesis_dag.to_bytes();
        let origin_header = self.headers.get(&origin)?.clone();
        let accounts = self.base_accounts.clone()?;
        let order = self.total_order();
        let suffix: Vec<DagBlock> = order
            .iter()
            .skip(1)
            .filter_map(|id| self.get_block(id))
            .collect();
        bincode::serialize(&SyncSnapshot {
            origin_header,
            accounts,
            suffix,
            proof: self.proof.clone(),
        })
        .ok()
    }

    /// Adopt a bootstrap snapshot from a pruned peer: re-anchor at its pruning
    /// point and apply the trusted suffix. Returns `false` (no-op) if we already
    /// have the snapshot's origin (caught up / ahead). Persists a compacted log.
    pub fn import_snapshot(&mut self, bytes: &[u8]) -> Result<bool, String> {
        let snap: SyncSnapshot = bincode::deserialize(bytes).map_err(|e| e.to_string())?;
        let origin = snap.origin_header.id();
        if self.blocks.contains(&origin) {
            return Ok(false);
        }
        // Trustless check: the NiPoPoW proof must certify, by PoW alone, that the
        // origin descends from our genesis on a real chain, and tell us its work.
        // Anchor at the original genesis (a bootstrapped node's genesis_dag has
        // moved to its current pruning point).
        let work = verify_proof(&snap.proof, origin, self.original_genesis)?;

        // Most-work selection: a fresh node (only genesis, never pruned) adopts; a
        // node already bootstrapped from a peer switches only to a strictly
        // greater-work proof; a node that built/pruned its own chain never adopts.
        let fresh = self.block_count() == 1 && !self.history_pruned;
        let stronger = self.bootstrapped && work > self.bootstrap_work;
        if !fresh && !stronger {
            return Ok(false);
        }

        self.load_snapshot(
            snap.origin_header.clone(),
            snap.accounts.clone(),
            snap.proof.clone(),
        );
        for block in &snap.suffix {
            self.apply(block)?; // trusted: no PoW / difficulty re-validation
        }
        // Persist a compacted log so a restart reproduces the synced chain.
        let mut records: Vec<Vec<u8>> = Vec::with_capacity(1 + snap.suffix.len());
        records.push(
            bincode::serialize(&LogRecord::Snapshot {
                origin_header: snap.origin_header,
                accounts: snap.accounts,
                proof: snap.proof,
            })
            .map_err(|e| e.to_string())?,
        );
        for block in &snap.suffix {
            records.push(
                bincode::serialize(&LogRecord::Block(block.clone())).map_err(|e| e.to_string())?,
            );
        }
        self.log.replace_all(&records).map_err(|e| e.to_string())?;
        self.bootstrapped = true;
        self.bootstrap_work = work;
        Ok(true)
    }

    /// Re-anchor the DAG at a finalized pruning point, discarding its entire past
    /// (headers + GHOSTDAG/reachability data, not only transaction bodies). The
    /// pruning point becomes the new origin and its account set the new genesis
    /// state. Returns the number of blocks pruned (0 if not deep enough). See
    /// `docs/pruning.md`.
    pub fn prune(&mut self) -> Result<usize, BankError> {
        let order = self.total_order(); // order[0] == current origin
        let n = order.len();
        // Retain at least an LWMA window + finality margin behind the tip so that
        // every new block's difficulty window stays within retained blocks (no
        // divergence from non-pruned peers).
        let retain = (self.lwma_window.saturating_add(self.finality_depth.max(1))) as usize;
        if n <= retain.saturating_add(1) {
            return Ok(0);
        }
        let cutoff = n - retain;

        // The selected chain (tip → current origin): the pruning point must lie
        // on it so the pruned prefix is exactly `past(P) ∪ {P}`.
        let mut on_selected_chain = HashSet::new();
        {
            let mut cur = self.engine.tip();
            loop {
                on_selected_chain.insert(cur.to_bytes());
                if cur == self.genesis_dag {
                    break;
                }
                cur = self.engine.selected_parent(cur);
            }
        }
        // Deepest selected-chain block at index ≤ cutoff (and past the origin).
        let p_idx = match (1..=cutoff.min(n - 1))
            .rev()
            .find(|&i| on_selected_chain.contains(&order[i]))
        {
            Some(i) => i,
            None => return Ok(0),
        };
        let p = order[p_idx];
        let p_dag = DagHash::from_bytes(p);

        // Build the retained NiPoPoW proof for P *before* its ancestors are
        // dropped (chaining onto any existing proof so it stays genesis-anchored).
        let new_proof = self.build_proof_for(p);

        // Base state = account set after executing the pruned prefix (order[..=p_idx]).
        // Normally this is seeded from genesis; after finalized-tx pruning it must be
        // rebuilt from the checkpoint prefix that still contains the authoritative
        // tx effects.
        let base_bank = self.open_bank(
            &self.checkpoint_dir,
            self.base_accounts.is_none() && !self.history_pruned,
        )?;
        if self.history_pruned && self.base_accounts.is_none() {
            let cp = self.checkpoint.as_ref().ok_or_else(|| {
                BankError::Storage("history pruned; no checkpoint available".into())
            })?;
            if cp.len > p_idx + 1 {
                return Err(BankError::Storage(
                    "prune point is before checkpoint".into(),
                ));
            }
            base_bank
                .accounts()
                .commit(cp.accounts.iter().cloned())
                .map_err(|e| BankError::Storage(e.to_string()))?;
            if cp.len <= p_idx {
                self.replay_blocks(&base_bank, &order[cp.len..=p_idx])?;
            }
        } else if let Some(base) = &self.base_accounts {
            base_bank
                .accounts()
                .commit(base.iter().cloned())
                .map_err(|e| BankError::Storage(e.to_string()))?;
            self.replay_blocks(&base_bank, &order[..=p_idx])?;
        } else {
            self.replay_blocks(&base_bank, &order[..=p_idx])?;
        }
        let new_base = base_bank
            .accounts()
            .dump()
            .map_err(|e| BankError::Storage(e.to_string()))?;

        // Rebuild the engine with P as genesis; re-add the kept suffix.
        let kept: HashSet<[u8; 32]> = order[p_idx..].iter().copied().collect();
        let engine = DagEngine::new(self.k, p_dag);
        engine.add_block(p_dag, &[blockhash::ORIGIN]);
        for id in &order[p_idx + 1..] {
            let header = self.headers.get(id).expect("kept block has a header");
            let mut parents: Vec<DagHash> = header
                .parents
                .iter()
                .map(|pp| {
                    if kept.contains(pp) {
                        DagHash::from_bytes(*pp)
                    } else {
                        p_dag
                    }
                })
                .collect();
            parents.sort_by(|a, b| a.to_bytes().cmp(&b.to_bytes()));
            parents.dedup();
            engine.add_block(DagHash::from_bytes(*id), &parents);
        }

        let pruned_count = self.blocks.len().saturating_sub(kept.len());

        let new_headers: HashMap<[u8; 32], DagBlockHeader> = kept
            .iter()
            .filter_map(|id| self.headers.get(id).cloned().map(|h| (*id, h)))
            .collect();
        let new_block_txs: HashMap<[u8; 32], Vec<Transaction>> = self
            .block_txs
            .iter()
            .filter_map(|(id, txs)| {
                if kept.contains(id) {
                    Some((*id, txs.clone()))
                } else {
                    None
                }
            })
            .collect();

        let p_header = new_headers
            .get(&p)
            .cloned()
            .expect("origin header retained");

        // Compact the durable log so the prune survives a restart: a leading
        // snapshot (origin header + base accounts + proof) then the kept suffix.
        let mut records: Vec<Vec<u8>> = Vec::with_capacity(n - p_idx);
        records.push(
            bincode::serialize(&LogRecord::Snapshot {
                origin_header: p_header,
                accounts: new_base.clone(),
                proof: new_proof.clone(),
            })
            .map_err(|e| BankError::Storage(e.to_string()))?,
        );
        for id in &order[p_idx + 1..] {
            if let Some(block) = self.get_block(id) {
                records.push(
                    bincode::serialize(&LogRecord::Block(block))
                        .map_err(|e| BankError::Storage(e.to_string()))?,
                );
            }
        }
        self.log
            .replace_all(&records)
            .map_err(|e| BankError::Storage(e.to_string()))?;

        self.engine = engine;
        self.genesis_dag = p_dag;
        self.base_accounts = Some(new_base);
        self.history_pruned = true;
        self.checkpoint = None;
        self.headers = new_headers;
        self.block_txs = new_block_txs;
        self.blocks = kept;
        self.state = None;
        self.executed.clear();
        self.tips = vec![self.engine.tip().to_bytes()];
        self.proof = new_proof;

        Ok(pruned_count)
    }

    /// Reconstruct a stored block by id (for serving peer backfill requests),
    /// or `None` if we don't have it. Genesis (no stored header txs) is included.
    pub fn get_block(&self, id: &[u8; 32]) -> Option<DagBlock> {
        let header = self.headers.get(id)?.clone();
        let transactions = self
            .block_txs
            .get(id)
            .map(|txs| {
                txs.iter()
                    .map(|t| {
                        bincode::serialize(t).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "serialize tx for block reply failed; dropping tx body");
                            Vec::new()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(DagBlock {
            header,
            transactions,
        })
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

    fn transfer(from: &solana_keypair::Keypair, to: &Pubkey, lamports: u64) -> Transaction {
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
            chain
                .mine(&[transfer(&payer, &a.pubkey(), 100_000_000)])
                .unwrap();
            chain
                .mine(&[transfer(&payer, &a.pubkey(), 50_000_000)])
                .unwrap();
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
        assert_eq!(
            bank.state_root().unwrap(),
            root_after_mining,
            "replay reproduces state"
        );
    }

    #[test]
    fn open_legacy_log_records() {
        let chain_dir = dir("legacy-log");
        let log_path = chain_dir.join("dag.log");
        std::fs::create_dir_all(&chain_dir).unwrap();

        let to = Pubkey::new_unique();
        let tx = transfer(&solana_keypair::Keypair::new(), &to, 1);
        let tx_bytes = vec![bincode::serialize(&tx).unwrap()];
        let legacy = LegacyDagBlock {
            header: LegacyDagBlockHeader {
                version: HEADER_VERSION,
                parents: vec![],
                timestamp: 1_750_000_000,
                tx_merkle_root: tx_merkle_root(&tx_bytes),
                target: easy_target(),
                nonce: 0,
                miner: Pubkey::new_unique(),
            },
            transactions: tx_bytes.clone(),
        };

        let log = BlockLog::open(&log_path).unwrap();
        let rec = bincode::serialize(&LegacyLogRecord::Block(legacy)).unwrap();
        log.append(&rec).unwrap();
        drop(log);

        let chain =
            DagChain::open(chain_dir, 3, easy_target(), Pubkey::new_unique(), 0, vec![]).unwrap();
        assert_eq!(chain.block_count(), 2, "genesis + legacy block loaded");
        let order = chain.total_order();
        assert_eq!(order.len(), 2);
        assert_eq!(
            chain.get_block(&order[1]).unwrap().header.version,
            HEADER_VERSION
        );
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
            chain
                .mine(&[transfer(&payer, &a.pubkey(), 100_000_000)])
                .unwrap();
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
            chain
                .mine(&[transfer(&a, &b.pubkey(), 10_000_000)])
                .unwrap();

            let bank = chain.rebuild_state().unwrap();
            (
                bank.state_root().unwrap(),
                bank.balance(&a.pubkey()),
                bank.balance(&b.pubkey()),
            )
        };

        let (root1, a_bal, b_bal) = run("merge1");
        assert_eq!(
            a_bal,
            100_000_000 - 10_000_000 - 5_000,
            "A funded before spend across the merge"
        );
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
        let b1 = c1
            .mine(&[transfer(&payer, &a.pubkey(), 100_000_000)])
            .unwrap();
        let b2 = c1
            .mine(&[transfer(&payer, &b.pubkey(), 50_000_000)])
            .unwrap();
        let b3 = c2
            .mine(&[transfer(&payer, &a.pubkey(), 10_000_000)])
            .unwrap();

        // Gossip: exchange each other's blocks (in dependency order).
        c2.accept(b1).unwrap();
        c2.accept(b2).unwrap();
        c1.accept(b3).unwrap();

        // Both nodes now hold the same block set → identical order and state.
        assert_eq!(
            c1.total_order(),
            c2.total_order(),
            "miners converge on one order"
        );
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
        assert_eq!(
            chain.next_target(),
            easy_target(),
            "warmup holds genesis target"
        );
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
        let mut chain = DagChain::open(
            dir("vstate"),
            3,
            easy_target(),
            miner,
            1_000_000,
            allocs.clone(),
        )
        .unwrap();

        let check = |chain: &mut DagChain| {
            let v = chain.virtual_state().unwrap().state_root().unwrap();
            let r = chain.rebuild_state().unwrap().state_root().unwrap();
            assert_eq!(v, r, "virtual state diverged from full replay");
        };

        chain
            .mine(&[transfer(&payer, &a.pubkey(), 100_000_000)])
            .unwrap();
        check(&mut chain); // first call: full rebuild
        chain
            .mine(&[transfer(&payer, &b.pubkey(), 50_000_000)])
            .unwrap();
        check(&mut chain); // append fast path

        // A parallel block off genesis, then a merge that spends from A.
        let block2 = {
            let tmp = DagChain::open(
                dir("vstate-side"),
                3,
                easy_target(),
                miner,
                1_000_000,
                allocs,
            )
            .unwrap();
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
            interlink: chain.interlink_for_parent(parent),
            nonce: 0,
            miner,
        };
        while !meets_target(&h.id(), &target) {
            h.nonce = h.nonce.wrapping_add(1);
        }
        chain
            .accept(DagBlock {
                header: h,
                transactions: vec![],
            })
            .unwrap();

        // The checkpoint prefix is untouched, so the rebuild runs from it.
        assert_eq!(
            chain.checkpoint.as_ref().unwrap().len,
            cp_len,
            "checkpoint unchanged"
        );
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
        assert_eq!(
            before, after,
            "state preserved after pruning finalized tx bodies"
        );
    }

    #[test]
    fn virtual_state_error_on_pruned_missing_checkpoint_base() {
        // If finalized tx bodies are pruned after checkpointing, `prune_finalized_transactions`
        // sets `history_pruned` with no in-memory base snapshot. A later order
        // reorg should fail fast instead of silently rebuilding from genesis.
        let payer = solana_keypair::Keypair::new();
        let mut chain = DagChain::open(
            dir("pruned-rebuild-fail"),
            3,
            easy_target(),
            Pubkey::new_unique(),
            1_000_000,
            vec![(payer.pubkey(), 1_000_000_000)],
        )
        .unwrap();
        chain.set_finality_depth(2);
        for i in 0..12 {
            let txs = if i < 2 {
                vec![transfer(&payer, &Pubkey::new_unique(), 50_000_000)]
            } else {
                vec![]
            };
            chain.mine(&txs).unwrap();
            chain.virtual_state().unwrap();
        }
        assert!(
            chain.checkpoint.is_some(),
            "checkpoint exists for this scenario"
        );
        let pruned = chain.prune_finalized_transactions().unwrap();
        assert!(
            pruned > 0,
            "prune_finalized_transactions removed prefix tx bodies"
        );
        assert!(chain.is_history_pruned());
        assert!(chain.base_accounts.is_none());
        let last_id = chain.checkpoint.as_ref().unwrap().last_id;
        chain.checkpoint.as_mut().unwrap().last_id = {
            let mut tamper = last_id;
            tamper[0] = tamper[0].wrapping_add(1);
            tamper
        };
        let err = match chain.virtual_state() {
            Ok(_) => panic!("expected virtual_state to fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("reorg below pruned history"),
            "unexpected error: {}",
            err.to_string()
        );
    }

    #[test]
    fn prune_after_finalize_tx_prune_uses_checkpoint_state() {
        // If finalized tx bodies were already removed, prune() must still be able
        // to rebuild the re-anchor base from checkpoint state.
        let payer = solana_keypair::Keypair::new();
        let mut chain = DagChain::open_with_daa(
            dir("prune-after-txpurge"),
            3,
            easy_target(),
            Pubkey::new_unique(),
            1_000_000,
            vec![(payer.pubkey(), 1_000_000_000)],
            10,
            2,
        )
        .unwrap();
        chain.set_finality_depth(2);
        let target = Pubkey::new_unique();
        for i in 0..12 {
            let txs = if i < 2 {
                vec![transfer(&payer, &target, 50_000_000)]
            } else {
                vec![]
            };
            chain.mine(&txs).unwrap();
            chain.virtual_state().unwrap();
        }
        let before = chain.virtual_state().unwrap().state_root().unwrap();
        assert!(chain.prune_finalized_transactions().unwrap() > 0);
        let pruned = chain.prune().unwrap();
        assert!(pruned > 0, "prune still succeeds after tx-body pruning");
        let after = chain.virtual_state().unwrap().state_root().unwrap();
        assert_eq!(
            before, after,
            "prune point base rebuild uses checkpoint snapshot"
        );
    }

    #[test]
    fn reanchor_prune_preserves_state_and_order() {
        // Re-anchoring at a finalized pruning point must preserve both the state
        // (INV1) and the post-pruning-point order (INV2). Uses DAA mode with a
        // small window so the prune triggers; on-time spacing keeps difficulty at
        // the easy genesis target (fast grind).
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let mut chain = DagChain::open_with_daa(
            dir("reanchor"),
            3,
            easy_target(),
            miner,
            1_000_000,
            vec![(payer.pubkey(), 1_000_000_000)],
            10,
            5,
        )
        .unwrap();
        chain.set_finality_depth(2);

        let mut ts = 1_000_000i64;
        for i in 0..24 {
            let txs = if i < 3 {
                vec![transfer(&payer, &a.pubkey(), 20_000_000)]
            } else {
                vec![]
            };
            chain.mine_at(&txs, ts).unwrap();
            ts += 10; // on time → difficulty holds easy
            chain.virtual_state().unwrap();
        }

        let pre_root = chain.virtual_state().unwrap().state_root().unwrap();
        let pre_order = chain.total_order();
        let pre_blocks = chain.block_count();

        let pruned = chain.prune().unwrap();
        assert!(pruned > 0, "re-anchor pruned some blocks");
        assert!(chain.is_history_pruned());
        assert!(chain.block_count() < pre_blocks, "fewer blocks retained");
        let post_order = chain.total_order();
        assert_eq!(
            chain.tips(),
            &[post_order.last().copied().expect("order non-empty")],
            "tips are recomputed from retained tip"
        );
        assert!(chain.tips().iter().all(|t| chain.has_block(t)));

        // INV1: state preserved.
        let post_root = chain.virtual_state().unwrap().state_root().unwrap();
        assert_eq!(
            pre_root, post_root,
            "INV1: state preserved across re-anchor"
        );
        // rebuild_state (now base-seeded) also reproduces it.
        assert_eq!(
            chain.rebuild_state().unwrap().state_root().unwrap(),
            pre_root,
            "rebuild from re-anchor base matches"
        );

        // INV2: the new order is exactly the old order from the pruning point on.
        let p = post_order[0];
        let p_idx = pre_order
            .iter()
            .position(|x| *x == p)
            .expect("P present in pre-order");
        assert_eq!(
            post_order.as_slice(),
            &pre_order[p_idx..],
            "INV2: order suffix preserved"
        );

        // The chain still works after pruning: mine more, state stays consistent.
        chain
            .mine_at(&[transfer(&payer, &a.pubkey(), 1_000_000)], ts)
            .unwrap();
        let r1 = chain.virtual_state().unwrap().state_root().unwrap();
        let r2 = chain.rebuild_state().unwrap().state_root().unwrap();
        assert_eq!(r1, r2, "post-prune mining keeps virtual == rebuild");
    }

    #[test]
    fn pruning_persists_across_restart() {
        // After a re-anchor prune, the compacted log must reproduce the pruned
        // chain on restart — same blocks, order, and state (not the full history).
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let d = dir("prune-restart");
        let open = |d: &PathBuf| {
            let mut c = DagChain::open_with_daa(
                d.clone(),
                3,
                easy_target(),
                miner,
                1_000_000,
                vec![(payer.pubkey(), 1_000_000_000)],
                10,
                5,
            )
            .unwrap();
            c.set_finality_depth(2);
            c
        };

        let (post_root, post_order, post_blocks) = {
            let mut chain = open(&d);
            let mut ts = 1_000_000i64;
            for i in 0..24 {
                let txs = if i < 3 {
                    vec![transfer(&payer, &a.pubkey(), 20_000_000)]
                } else {
                    vec![]
                };
                chain.mine_at(&txs, ts).unwrap();
                ts += 10;
                chain.virtual_state().unwrap();
            }
            assert!(chain.prune().unwrap() > 0);
            let root = chain.virtual_state().unwrap().state_root().unwrap();
            (root, chain.total_order(), chain.block_count())
        };

        // Restart: reopen the same data dir; the compacted log re-anchors.
        let mut reopened = open(&d);
        assert!(
            reopened.is_history_pruned(),
            "restart restores pruned state"
        );
        assert_eq!(
            reopened.block_count(),
            post_blocks,
            "only the kept suffix is reloaded"
        );
        assert_eq!(
            reopened.total_order(),
            post_order,
            "order reproduced after restart"
        );
        assert_eq!(
            reopened.virtual_state().unwrap().state_root().unwrap(),
            post_root,
            "state reproduced after restart"
        );
        // And it keeps working: mine on top.
        reopened
            .mine_at(&[transfer(&payer, &a.pubkey(), 1_000_000)], 2_000_000)
            .unwrap();
        let v = reopened.virtual_state().unwrap().state_root().unwrap();
        let r = reopened.rebuild_state().unwrap().state_root().unwrap();
        assert_eq!(v, r, "post-restart mining stays consistent");
    }

    #[test]
    fn snapshot_sync_bootstraps_a_fresh_node() {
        // A pruned node bootstraps a fresh node from its snapshot: the fresh node
        // reaches the identical order and state without ever seeing pruned history.
        let payer = solana_keypair::Keypair::new();
        let a = solana_keypair::Keypair::new();
        let miner = Pubkey::new_unique();
        let allocs = vec![(payer.pubkey(), 1_000_000_000)];

        let mut node1 = DagChain::open_with_daa(
            dir("snap-src"),
            3,
            easy_target(),
            miner,
            1_000_000,
            allocs.clone(),
            10,
            5,
        )
        .unwrap();
        node1.set_finality_depth(2);
        let mut ts = 1_000_000i64;
        for i in 0..24 {
            let txs = if i < 3 {
                vec![transfer(&payer, &a.pubkey(), 20_000_000)]
            } else {
                vec![]
            };
            node1.mine_at(&txs, ts).unwrap();
            ts += 10;
            node1.virtual_state().unwrap();
        }
        assert!(node1.prune().unwrap() > 0);
        let snap = node1
            .export_snapshot()
            .expect("pruned node exports a snapshot");
        let want_order = node1.total_order();
        let want_root = node1.virtual_state().unwrap().state_root().unwrap();

        // Fresh node: only genesis until it adopts the snapshot.
        let mut node2 = DagChain::open_with_daa(
            dir("snap-dst"),
            3,
            easy_target(),
            miner,
            1_000_000,
            allocs,
            10,
            5,
        )
        .unwrap();
        node2.set_finality_depth(2);
        assert!(
            node2.import_snapshot(&snap).unwrap(),
            "fresh node adopts the snapshot"
        );
        assert!(node2.is_history_pruned());
        assert_eq!(
            node2.total_order(),
            want_order,
            "bootstrapped order matches"
        );
        assert_eq!(
            node2.virtual_state().unwrap().state_root().unwrap(),
            want_root,
            "bootstrapped state matches"
        );
        // Re-importing is a no-op (already have the origin).
        assert!(!node2.import_snapshot(&snap).unwrap());
    }

    #[test]
    fn most_work_proof_wins() {
        // A fresh node ends up on the higher-work chain regardless of the order in
        // which competing peers' snapshots arrive; a lower-work proof is refused.
        let miner = Pubkey::new_unique();
        let build = |tag: &str, n: usize| {
            let mut c = DagChain::open_with_daa(dir(tag), 3, easy_target(), miner, 0, vec![], 10, 5)
                .unwrap();
            c.set_finality_depth(2);
            // Mine after the genesis timestamp so solve times are uniform (no
            // negative-solvetime difficulty bump): both chains share difficulty, so
            // the longer one certifies strictly more accumulated work.
            let mut ts = 1_750_000_010i64;
            for _ in 0..n {
                c.mine_at(&[], ts).unwrap();
                ts += 10;
            }
            c.prune().unwrap();
            c
        };
        let small = build("mw-small", 16);
        let large = build("mw-large", 48);
        let snap_small = small.export_snapshot().unwrap();
        let snap_large = large.export_snapshot().unwrap();

        let fresh = |tag: &str| {
            let mut c = DagChain::open_with_daa(dir(tag), 3, easy_target(), miner, 0, vec![], 10, 5)
                .unwrap();
            c.set_finality_depth(2);
            c
        };

        // small then large → upgrades to the higher-work chain.
        let mut n1 = fresh("mw-n1");
        assert!(n1.import_snapshot(&snap_small).unwrap(), "fresh node adopts first proof");
        assert!(n1.import_snapshot(&snap_large).unwrap(), "switches to the stronger proof");
        assert_eq!(n1.total_order(), large.total_order(), "n1 ends on the large chain");

        // large then small → keeps the higher-work chain.
        let mut n2 = fresh("mw-n2");
        assert!(n2.import_snapshot(&snap_large).unwrap());
        assert!(!n2.import_snapshot(&snap_small).unwrap(), "weaker proof refused");
        assert_eq!(n2.total_order(), large.total_order(), "n2 keeps the large chain");
    }

    #[test]
    fn import_rejects_invalid_proof() {
        // A bootstrap snapshot whose NiPoPoW proof doesn't verify is refused.
        let miner = Pubkey::new_unique();
        let mut node1 = DagChain::open_with_daa(
            dir("badproof-src"),
            3,
            easy_target(),
            miner,
            0,
            vec![],
            10,
            5,
        )
        .unwrap();
        node1.set_finality_depth(2);
        let mut ts = 1_000_000i64;
        for _ in 0..24 {
            node1.mine_at(&[], ts).unwrap();
            ts += 10;
            node1.virtual_state().unwrap();
        }
        node1.prune().unwrap();
        let good = node1.export_snapshot().unwrap();

        // Tamper: drop the genesis anchor so the proof is no longer anchored.
        let mut snap: SyncSnapshot = bincode::deserialize(&good).unwrap();
        snap.proof.remove(0);
        let bad = bincode::serialize(&snap).unwrap();

        let mut node2 = DagChain::open_with_daa(
            dir("badproof-dst"),
            3,
            easy_target(),
            miner,
            0,
            vec![],
            10,
            5,
        )
        .unwrap();
        node2.set_finality_depth(2);
        assert!(
            node2.import_snapshot(&bad).is_err(),
            "tampered proof must be rejected"
        );
        assert!(
            !node2.is_history_pruned(),
            "rejected import left the node untouched"
        );
    }

    #[test]
    fn reanchor_preserves_difficulty_window() {
        // INV3: a pruned node and a non-pruned node compute the SAME next target
        // (the prune retained a full LWMA window), so they cannot diverge.
        let miner = Pubkey::new_unique();
        let build = |tag: &str| {
            let mut c =
                DagChain::open_with_daa(dir(tag), 3, easy_target(), miner, 0, vec![], 10, 5)
                    .unwrap();
            c.set_finality_depth(2);
            let mut ts = 500_000i64;
            for _ in 0..24 {
                c.mine_at(&[], ts).unwrap();
                ts += 10;
            }
            c
        };
        let mut pruned = build("daawin-a");
        let intact = build("daawin-b");
        // Identical mining sequences ⇒ identical chains ⇒ identical next target.
        assert_eq!(
            pruned.next_target(),
            intact.next_target(),
            "sanity: chains identical"
        );

        pruned.prune().unwrap();
        assert!(pruned.is_history_pruned());
        assert_eq!(
            pruned.next_target(),
            intact.next_target(),
            "INV3: pruned node computes the same difficulty as a non-pruned peer"
        );
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
        let mined = chain
            .mine(&[transfer(&payer, &a.pubkey(), 100_000_000)])
            .unwrap();
        let served = chain.get_block(&mined.id()).expect("have the block");
        assert_eq!(served.id(), mined.id(), "reconstructed block id matches");
        assert_eq!(served.transactions, mined.transactions, "txs round-trip");
        assert!(chain.has_block(&mined.id()));
        assert!(chain.get_block(&[9u8; 32]).is_none(), "unknown id → None");
    }

    #[test]
    fn rejects_forged_interlink() {
        // A block whose interlink isn't correctly derived from its selected
        // parent is rejected (interlinks are PoW-committed and validated).
        let miner = Pubkey::new_unique();
        let mut chain =
            DagChain::open(dir("forge-il"), 3, easy_target(), miner, 0, vec![]).unwrap();
        chain.mine(&[]).unwrap();
        chain.mine(&[]).unwrap();
        let mut block = chain.build_block(&[]); // correct interlink
        block.header.interlink.push([0x42; 32]); // forge an extra pointer
        while !meets_target(&block.header.id(), &block.header.target) {
            block.header.nonce = block.header.nonce.wrapping_add(1);
        }
        let err = chain.accept(block).unwrap_err();
        assert!(
            err.contains("interlink"),
            "expected interlink rejection, got: {err}"
        );
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
        assert!(
            err.contains("unexpected difficulty"),
            "expected difficulty rejection, got: {err}"
        );
        assert_eq!(
            chain.block_count(),
            1,
            "the lowballed block was not accepted"
        );
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
        assert!(
            wn >= wg * primitive_types::U256::from(95u64) / primitive_types::U256::from(100u64)
        );
        assert!(
            wn <= wg * primitive_types::U256::from(105u64) / primitive_types::U256::from(100u64)
        );
    }
}
