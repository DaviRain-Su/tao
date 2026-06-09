//! Bank — executes Solana transactions via the embedded SVM.
//!
//! Wraps a [`TransactionBatchProcessor`] over our [`AccountsDb`]. For M2 we
//! execute transactions one at a time (committing between them) so sequential
//! intra-block semantics are simple and correct; batched parallel execution is
//! a later optimization. See `docs/svm-integration-4.0.md` for the API contract.

use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

use solana_account::{Account, AccountSharedData, ReadableAccount};
use solana_clock::{Clock, Epoch, Slot};
use solana_compute_budget::compute_budget_limits::ComputeBudgetLimits;
use solana_fee_structure::FeeDetails;
use solana_hash::Hash;
use solana_program_runtime::loaded_programs::{
    BlockRelation, ForkGraph, ProgramCacheEntry,
};
use solana_program_runtime::solana_sbpf::{program::BuiltinProgram, vm::Config as VmConfig};
use solana_pubkey::Pubkey;
use solana_svm::account_loader::CheckedTransactionDetails;
use solana_svm::transaction_processing_result::ProcessedTransaction;
use solana_svm::transaction_processor::{
    ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
    TransactionProcessingEnvironment,
};
use solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback};
use solana_svm_feature_set::SVMFeatureSet;
use solana_transaction::sanitized::SanitizedTransaction;
use solana_transaction::Transaction;

use tao_database::AccountsDb;

/// Lamports charged per signature (matches Solana's default base fee).
pub const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

/// A trivial non-forking fork graph (our chain is linear in M2).
pub struct TaoForkGraph;

impl ForkGraph for TaoForkGraph {
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
        match a.cmp(&b) {
            Ordering::Less => BlockRelation::Ancestor,
            Ordering::Equal => BlockRelation::Equal,
            Ordering::Greater => BlockRelation::Descendant,
        }
    }
}

/// Account-loading callback backed by the persistent [`AccountsDb`].
struct DbCallback<'a> {
    db: &'a AccountsDb,
}

impl InvokeContextCallback for DbCallback<'_> {}

impl TransactionProcessingCallback for DbCallback<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        match self.db.get(pubkey) {
            Ok(Some(account)) => Some((account, 0)),
            Ok(None) => None,
            Err(e) => {
                tracing::error!(%pubkey, error = %e, "account load failed");
                None
            }
        }
    }
}

/// Outcome of executing a single transaction.
#[derive(Debug, Clone)]
pub struct TxOutcome {
    /// Whether the transaction committed successfully (program returned Ok).
    pub succeeded: bool,
    /// Total fee charged (transaction + priority), in lamports.
    pub fee: u64,
    /// Error string when the transaction failed or was dropped.
    pub error: Option<String>,
}

/// Errors that prevent a transaction from being processed at all.
#[derive(Debug, thiserror::Error)]
pub enum BankError {
    #[error("signature verification failed: {0}")]
    BadSignature(String),
    #[error("storage error: {0}")]
    Storage(String),
}

/// The execution Bank.
pub struct Bank {
    db: Arc<AccountsDb>,
    processor: TransactionBatchProcessor<TaoForkGraph>,
    // Kept alive for the processor's weak reference.
    _fork_graph: Arc<RwLock<TaoForkGraph>>,
    slot: Slot,
    epoch: Epoch,
    feature_set: SVMFeatureSet,
}

impl Bank {
    /// Build a Bank over `db` at the given slot, registering the System builtin
    /// and sysvars so transactions can execute.
    pub fn new(db: Arc<AccountsDb>, slot: Slot) -> Self {
        let epoch: Epoch = 0;
        let fork_graph = Arc::new(RwLock::new(TaoForkGraph));

        let processor = TransactionBatchProcessor::<TaoForkGraph>::new(
            slot,
            epoch,
            Arc::downgrade(&fork_graph),
            Some(Arc::new(BuiltinProgram::new_loader(VmConfig::default()))),
            None,
        );

        let bank = Self {
            db,
            processor,
            _fork_graph: fork_graph,
            slot,
            epoch,
            feature_set: SVMFeatureSet::all_enabled(),
        };
        bank.install_sysvars();
        bank.register_system_builtin();
        bank
    }

    fn install_sysvars(&self) {
        let clock = Clock {
            slot: self.slot,
            epoch_start_timestamp: 0,
            epoch: self.epoch,
            leader_schedule_epoch: self.epoch,
            unix_timestamp: 0,
        };
        self.put_sysvar(solana_sdk_ids::sysvar::clock::id(), &clock);
        self.put_sysvar(solana_sdk_ids::sysvar::rent::id(), &solana_rent::Rent::default());
        self.put_sysvar(
            solana_sdk_ids::sysvar::epoch_schedule::id(),
            &solana_epoch_schedule::EpochSchedule::without_warmup(),
        );

        let callback = DbCallback { db: &self.db };
        self.processor.fill_missing_sysvar_cache_entries(&callback);
    }

    fn put_sysvar<T: serde::Serialize>(&self, id: Pubkey, value: &T) {
        let data = bincode::serialize(value).expect("sysvar serialization is infallible");
        let mut account = AccountSharedData::new(1, data.len(), &solana_sdk_ids::sysvar::id());
        account.set_data_from_slice(&data);
        let _ = self.db.set(&id, &account);
    }

    fn register_system_builtin(&self) {
        let program_id = solana_sdk_ids::system_program::id();
        let name = "system_program";

        let account = AccountSharedData::from(Account {
            lamports: 1,
            data: name.as_bytes().to_vec(),
            owner: solana_sdk_ids::native_loader::id(),
            executable: true,
            rent_epoch: 0,
        });
        let _ = self.db.set(&program_id, &account);

        self.processor.add_builtin(
            program_id,
            ProgramCacheEntry::new_builtin(
                0,
                name.len(),
                solana_system_program::system_processor::Entrypoint::vm,
            ),
        );
    }

    /// Latest account-set state root (delegated to the store).
    pub fn state_root(&self) -> Result<[u8; 32], BankError> {
        self.db.state_root().map_err(|e| BankError::Storage(e.to_string()))
    }

    fn build_check_result(
        &self,
        sanitized: &SanitizedTransaction,
    ) -> solana_svm::account_loader::TransactionCheckResult {
        let sig_count = sanitized.signatures().len() as u64;
        let fee_details = FeeDetails::new(sig_count.saturating_mul(LAMPORTS_PER_SIGNATURE), 0);

        let cb = ComputeBudgetLimits::default();
        let limits = cb.get_compute_budget_and_limits(
            cb.loaded_accounts_bytes,
            fee_details,
            self.feature_set.formalize_loaded_transaction_data_size,
        );
        Ok(CheckedTransactionDetails::new(None, limits))
    }

    /// Execute and commit a single transaction. Fees are deducted from the fee
    /// payer by the SVM; the caller credits them to the miner (coinbase).
    pub fn execute_transaction(
        &self,
        tx: &Transaction,
        blockhash: Hash,
    ) -> Result<TxOutcome, BankError> {
        tx.verify().map_err(|e| BankError::BadSignature(e.to_string()))?;

        let sanitized = SanitizedTransaction::from_transaction_for_tests(tx.clone());
        let callback = DbCallback { db: &self.db };
        let check_results = vec![self.build_check_result(&sanitized)];

        let environment = TransactionProcessingEnvironment {
            blockhash,
            blockhash_lamports_per_signature: LAMPORTS_PER_SIGNATURE,
            epoch_total_stake: 0,
            feature_set: self.feature_set.clone(),
            program_runtime_environments_for_execution: self
                .processor
                .get_environments_for_epoch(self.epoch),
            program_runtime_environments_for_deployment: self
                .processor
                .get_environments_for_epoch(self.epoch),
            rent: solana_rent::Rent::default(),
        };
        let config = TransactionProcessingConfig {
            recording_config: ExecutionRecordingConfig::new_single_setting(false),
            ..Default::default()
        };

        let output = self.processor.load_and_execute_sanitized_transactions(
            &callback,
            &[sanitized.clone()],
            check_results,
            &environment,
            &config,
        );

        self.apply_result(&sanitized, output.processing_results.into_iter().next().unwrap())
    }

    fn apply_result(
        &self,
        sanitized: &SanitizedTransaction,
        result: solana_svm::transaction_processing_result::TransactionProcessingResult,
    ) -> Result<TxOutcome, BankError> {
        match result {
            Ok(ProcessedTransaction::Executed(executed)) => {
                let fee = executed.loaded_transaction.fee_details.total_fee();
                let succeeded = executed.execution_details.status.is_ok();
                if succeeded {
                    let writes = executed
                        .loaded_transaction
                        .accounts
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| sanitized.message().is_writable(*i))
                        .map(|(_, (pk, acct))| (pk, acct));
                    self.db.commit(writes).map_err(|e| BankError::Storage(e.to_string()))?;
                    Ok(TxOutcome { succeeded: true, fee, error: None })
                } else {
                    // Program failed: only fee/nonce rollback persists.
                    self.commit_rollback(executed.loaded_transaction.rollback_accounts)?;
                    Ok(TxOutcome {
                        succeeded: false,
                        fee,
                        error: Some(format!("{:?}", executed.execution_details.status)),
                    })
                }
            }
            Ok(ProcessedTransaction::FeesOnly(fees_only)) => {
                let fee = fees_only.fee_details.total_fee();
                self.commit_rollback(fees_only.rollback_accounts)?;
                Ok(TxOutcome {
                    succeeded: false,
                    fee,
                    error: Some(format!("{:?}", fees_only.load_error)),
                })
            }
            Err(e) => Ok(TxOutcome { succeeded: false, fee: 0, error: Some(format!("{e:?}")) }),
        }
    }

    fn commit_rollback(
        &self,
        rollback: solana_svm::rollback_accounts::RollbackAccounts,
    ) -> Result<(), BankError> {
        // `iter()` yields the fee-payer (and nonce, if any) keyed accounts to
        // persist when a transaction is charged fees but not committed.
        let changes: Vec<(Pubkey, AccountSharedData)> =
            rollback.iter().map(|(addr, acct)| (*addr, acct.clone())).collect();
        if !changes.is_empty() {
            self.db.commit(changes).map_err(|e| BankError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    /// Read an account's lamport balance (0 if absent).
    pub fn balance(&self, pubkey: &Pubkey) -> u64 {
        self.db.get(pubkey).ok().flatten().map(|a| a.lamports()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_keypair::Keypair;
    use solana_signer::Signer;

    fn tmp_db(tag: &str) -> Arc<AccountsDb> {
        let dir = std::env::temp_dir().join(format!("tao-bank-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(AccountsDb::open(&dir).unwrap())
    }

    fn fund(db: &AccountsDb, key: &Pubkey, lamports: u64) {
        db.set(key, &AccountSharedData::new(lamports, 0, &solana_sdk_ids::system_program::id()))
            .unwrap();
    }

    #[test]
    fn executes_system_transfer() {
        let db = tmp_db("transfer");
        let payer = Keypair::new();
        let recipient = Keypair::new();
        // Fund well above the rent-exempt minimum; amounts must keep both the
        // payer and the (new) recipient rent-exempt (all features enabled).
        fund(&db, &payer.pubkey(), 1_000_000_000);

        let bank = Bank::new(db.clone(), 1);
        let blockhash = Hash::new_unique();
        let amount = 5_000_000;
        let ix = solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &recipient.pubkey(),
            amount,
        );
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );

        let outcome = bank.execute_transaction(&tx, blockhash).unwrap();
        assert!(outcome.succeeded, "transfer failed: {:?}", outcome.error);
        assert_eq!(outcome.fee, LAMPORTS_PER_SIGNATURE);
        assert_eq!(bank.balance(&recipient.pubkey()), amount);
        assert_eq!(
            bank.balance(&payer.pubkey()),
            1_000_000_000 - amount - LAMPORTS_PER_SIGNATURE
        );
    }

    #[test]
    fn two_banks_reach_same_state_root() {
        let payer = Keypair::new();
        let recipient = Keypair::new();
        let blockhash = Hash::new_unique();
        let ix = solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &recipient.pubkey(),
            5_000_000,
        );
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

        let run = |tag: &str| {
            let db = tmp_db(tag);
            fund(&db, &payer.pubkey(), 1_000_000_000);
            let bank = Bank::new(db.clone(), 1);
            bank.execute_transaction(&tx, blockhash).unwrap();
            bank.state_root().unwrap()
        };

        assert_eq!(run("a"), run("b"));
    }
}
