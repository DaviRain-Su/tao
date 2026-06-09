//! `tao-dagvm` — execute a blockDAG's transactions through the SVM in GHOSTDAG
//! linear order.
//!
//! This ties together the two halves of the Solana-compatible blockDAG:
//! - [`tao_ghostdag::DagEngine`] orders the DAG (the "Case B" linearization), and
//! - [`tao_runtime::Bank`] executes transactions through the embedded SVM.
//!
//! A node would: gossip multi-parent blocks → add them here → on each tip change,
//! execute the linear order through the Bank and commit the resulting state. The
//! single agreed total order is what lets the SVM (which needs one order) run on
//! a blockDAG unchanged.

mod dag_chain;
pub use dag_chain::DagChain;
// The DAG block types now live in the consensus layer (unified with the linear
// header); re-exported here for convenience.
pub use tao_consensus::{DagBlock, DagBlockHeader};

use std::collections::HashMap;
use std::sync::Arc;

use solana_hash::Hash as Blockhash;
use solana_transaction::Transaction;
use tao_database::AccountsDb;
use tao_ghostdag::{blockhash, DagEngine, Hash as DagHash};
use tao_runtime::{Bank, BankError};

/// A blockDAG with transactions, executed through the SVM in GHOSTDAG order.
pub struct DagVm {
    engine: DagEngine,
    bank: Bank,
    block_txs: HashMap<DagHash, Vec<Transaction>>,
}

impl DagVm {
    /// Create a DAG VM with anticone bound `k`, `genesis` block id, and the
    /// account store the SVM executes against. The genesis block (no
    /// transactions) is added automatically.
    pub fn new(k: u16, genesis: DagHash, accounts: Arc<AccountsDb>) -> Self {
        let bank = Bank::new(accounts, 0);
        let engine = DagEngine::new(k, genesis);
        engine.add_block(genesis, &[blockhash::ORIGIN]);
        Self {
            engine,
            bank,
            block_txs: HashMap::new(),
        }
    }

    /// Add a block referencing `parents`, carrying `transactions`.
    pub fn add_block(
        &mut self,
        hash: DagHash,
        parents: &[DagHash],
        transactions: Vec<Transaction>,
    ) {
        self.engine.add_block(hash, parents);
        if !transactions.is_empty() {
            self.block_txs.insert(hash, transactions);
        }
    }

    /// The GHOSTDAG total order up to the current tip.
    pub fn total_order(&self) -> Vec<DagHash> {
        self.engine.total_order()
    }

    /// Execute every block's transactions in GHOSTDAG linear order through the
    /// SVM, returning the resulting account-set state root. Deterministic: the
    /// same DAG always yields the same order and the same state root.
    pub fn execute_linear(&self, env_blockhash: Blockhash) -> Result<[u8; 32], BankError> {
        for block in self.engine.total_order() {
            if let Some(txs) = self.block_txs.get(&block) {
                for tx in txs {
                    // A failed transaction (e.g. a double-spend later in the
                    // order) is skipped; its effects simply don't apply.
                    let _ = self.bank.execute_transaction(tx, env_blockhash.clone())?;
                }
            }
        }
        self.bank.state_root()
    }

    /// Access the underlying bank (for balance queries).
    pub fn bank(&self) -> &Bank {
        &self.bank
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account::AccountSharedData;
    use solana_keypair::Keypair;
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;

    fn tmp_db(tag: &str) -> Arc<AccountsDb> {
        let dir = std::env::temp_dir().join(format!("tao-dagvm-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(AccountsDb::open(&dir).unwrap())
    }

    fn fund(db: &AccountsDb, key: &Pubkey, lamports: u64) {
        db.set(
            key,
            &AccountSharedData::new(lamports, 0, &solana_sdk_ids::system_program::id()),
        )
        .unwrap();
    }

    fn transfer(from: &Keypair, to: &Pubkey, lamports: u64, bh: Blockhash) -> Transaction {
        let ix = solana_system_interface::instruction::transfer(&from.pubkey(), to, lamports);
        Transaction::new_signed_with_payer(&[ix], Some(&from.pubkey()), &[from], bh)
    }

    /// A blockDAG with a cross-block dependency (A is funded in one block, then
    /// spends in a later block that merges two parents). The GHOSTDAG order must
    /// place the funding before the spend, and execution must be deterministic.
    #[test]
    fn blockdag_linearized_svm_execution_is_deterministic() {
        let bh = Blockhash::new_unique();
        let payer = Keypair::new();
        let a = Keypair::new();
        let b = Keypair::new();

        let run = |tag: &str| {
            let db = tmp_db(tag);
            fund(&db, &payer.pubkey(), 1_000_000_000);
            let mut vm = DagVm::new(3, DagHash::from(1), db);
            // 2 and 3 are parallel children of genesis; 4 merges both.
            vm.add_block(
                DagHash::from(2),
                &[DagHash::from(1)],
                vec![transfer(&payer, &a.pubkey(), 100_000_000, bh)],
            );
            vm.add_block(
                DagHash::from(3),
                &[DagHash::from(1)],
                vec![transfer(&payer, &b.pubkey(), 50_000_000, bh)],
            );
            // Block 4 spends from A — only valid because block 2 (funding A) is
            // ordered before it by GHOSTDAG.
            vm.add_block(
                DagHash::from(4),
                &[DagHash::from(2), DagHash::from(3)],
                vec![transfer(&a, &b.pubkey(), 10_000_000, bh)],
            );
            let root = vm.execute_linear(bh).unwrap();
            (
                root,
                vm.bank().balance(&a.pubkey()),
                vm.bank().balance(&b.pubkey()),
                vm.bank().balance(&payer.pubkey()),
            )
        };

        let (root1, a_bal, b_bal, payer_bal) = run("run1");
        assert_eq!(
            a_bal,
            100_000_000 - 10_000_000 - 5_000,
            "A: funded then spent + fee"
        );
        assert_eq!(b_bal, 60_000_000, "B: from payer + from A");
        assert_eq!(
            payer_bal,
            1_000_000_000 - 150_000_000 - 2 * 5_000,
            "payer: two transfers + fees"
        );

        let (root2, ..) = run("run2");
        assert_eq!(
            root1, root2,
            "same DAG → same linear order → same state root"
        );
    }
}
