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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use solana_account::AccountSharedData;
use solana_hash::Hash as Blockhash;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use tao_consensus::{
    meets_target, next_target, tx_merkle_root, BlockHeader as ConsHeader, DifficultyParams, Target,
};
use tao_database::{AccountsDb, BlockLog};
use tao_ghostdag::{blockhash, DagEngine, Hash as DagHash};
use tao_runtime::{Bank, BankError};

const HEADER_VERSION: u32 = 1;
/// Fixed env blockhash for SVM execution (does not affect state for plain transfers).
fn env_blockhash() -> Blockhash {
    Blockhash::new_from_array([7u8; 32])
}

/// A multi-parent blockDAG header.
#[derive(Clone, Serialize, Deserialize)]
pub struct DagBlockHeader {
    pub version: u32,
    pub parents: Vec<[u8; 32]>,
    pub timestamp: i64,
    pub tx_merkle_root: [u8; 32],
    pub target: [u8; 32],
    pub nonce: u64,
    pub miner: [u8; 32],
}

impl DagBlockHeader {
    fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).expect("header serialization is infallible")
    }
    /// Block id = BLAKE3 of the header (also the PoW hash for this Blake3 PoW).
    pub fn id(&self) -> [u8; 32] {
        *blake3::hash(&self.serialize()).as_bytes()
    }
}

/// A DAG block: header + serialized transactions.
#[derive(Clone, Serialize, Deserialize)]
pub struct DagBlock {
    pub header: DagBlockHeader,
    pub transactions: Vec<Vec<u8>>,
}

impl DagBlock {
    pub fn id(&self) -> [u8; 32] {
        self.header.id()
    }
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
}

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
            tx_merkle_root: [0u8; 32],
            target: genesis_target,
            nonce: 0,
            miner: [0u8; 32],
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
            tx_merkle_root: tx_merkle_root(&serialized).to_bytes(),
            target,
            nonce: 0,
            miner: self.miner.to_bytes(),
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

    /// Rebuild the SVM state by executing the GHOSTDAG total order from genesis,
    /// returning a `Bank` over a freshly-built account store. Correct under
    /// parallel blocks / reorgs.
    pub fn rebuild_state(&self) -> Result<Bank, BankError> {
        let _ = std::fs::remove_dir_all(&self.accounts_dir);
        let db = Arc::new(
            AccountsDb::open(&self.accounts_dir).map_err(|e| BankError::Storage(e.to_string()))?,
        );
        let system = solana_sdk_ids::system_program::id();
        for (pubkey, lamports) in &self.allocations {
            db.set(pubkey, &AccountSharedData::new(*lamports, 0, &system))
                .map_err(|e| BankError::Storage(e.to_string()))?;
        }
        let bank = Bank::new(db, 0);

        let bh = env_blockhash();
        for block in self.engine.total_order() {
            if block == self.genesis_dag {
                continue;
            }
            if self.reward > 0 {
                bank.airdrop(&self.miner, self.reward)?;
            }
            if let Some(txs) = self.block_txs.get(&block.to_bytes()) {
                for tx in txs {
                    let _ = bank.execute_transaction(tx, bh.clone())?;
                }
            }
        }
        Ok(bank)
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
