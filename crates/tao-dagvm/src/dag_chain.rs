//! `DagChain` — a single-node blockDAG consensus core: multi-parent PoW blocks,
//! durable persistence, replay, and **correct merge handling**.
//!
//! State is a *derived cache*: it is (re)computed by executing the GHOSTDAG
//! total order through the SVM from genesis. This is O(history) per query but
//! always correct under parallel blocks / reorgs (the linear node uses the same
//! "rebuild from the log" approach). Incremental virtual-state processing is the
//! production optimization.
//!
//! Difficulty is fixed (from genesis) for now; a sampled-window DAA for high
//! block rates is future work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use solana_account::AccountSharedData;
use solana_hash::Hash as Blockhash;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use tao_consensus::{meets_target, tx_merkle_root, Target};
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
    genesis_dag: DagHash,
    target: Target,
    miner: Pubkey,
    reward: u64,
    accounts_dir: PathBuf,
    allocations: Vec<(Pubkey, u64)>,
}

impl DagChain {
    /// Open (or create) a DAG chain under `data_dir`. The block log is replayed.
    pub fn open(
        data_dir: PathBuf,
        k: u16,
        target: Target,
        miner: Pubkey,
        reward: u64,
        allocations: Vec<(Pubkey, u64)>,
    ) -> Result<Self, String> {
        // Deterministic genesis header (no parents, no txs).
        let genesis_header = DagBlockHeader {
            version: HEADER_VERSION,
            parents: vec![],
            timestamp: 1_750_000_000,
            tx_merkle_root: [0u8; 32],
            target,
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
            genesis_dag,
            target,
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

    fn now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Build a PoW block referencing the current tips with the given transactions.
    pub fn build_block(&self, transactions: &[Transaction]) -> DagBlock {
        let serialized: Vec<Vec<u8>> =
            transactions.iter().map(|t| bincode::serialize(t).expect("tx serialize")).collect();
        let mut header = DagBlockHeader {
            version: HEADER_VERSION,
            parents: self.tips.clone(),
            timestamp: Self::now(),
            tx_merkle_root: tx_merkle_root(&serialized).to_bytes(),
            target: self.target,
            nonce: 0,
            miner: self.miner.to_bytes(),
        };
        while !meets_target(&header.id(), &self.target) {
            header.nonce = header.nonce.wrapping_add(1);
        }
        DagBlock { header, transactions: serialized }
    }

    /// Mine a block (build + persist + apply).
    pub fn mine(&mut self, transactions: &[Transaction]) -> Result<DagBlock, String> {
        let block = self.build_block(transactions);
        self.log.append(&bincode::serialize(&block).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        self.apply(&block)?;
        Ok(block)
    }

    /// Accept an externally-produced block (e.g. from a peer): validate PoW,
    /// persist, and apply.
    pub fn accept(&mut self, block: DagBlock) -> Result<(), String> {
        if !meets_target(&block.header.id(), &block.header.target) {
            return Err("invalid proof-of-work".into());
        }
        if !block.header.parents.iter().all(|p| self.blocks.contains(p)) {
            return Err("unknown parent (orphan)".into());
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
}
