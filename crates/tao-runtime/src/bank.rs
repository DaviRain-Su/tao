//! Bank — executes Solana transactions via the embedded SVM.
//!
//! Wraps a [`TransactionBatchProcessor`] over our [`AccountsDb`]. For M2 we
//! execute transactions one at a time (committing between them) so sequential
//! intra-block semantics are simple and correct; batched parallel execution is
//! a later optimization. See `docs/svm-integration-4.0.md` for the API contract.

use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

use solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount};
use solana_clock::{Clock, Epoch, Slot};
use solana_compute_budget::compute_budget_limits::ComputeBudgetLimits;
use solana_fee_structure::FeeDetails;
use solana_hash::Hash;
use solana_program_runtime::execution_budget::SVMTransactionExecutionBudget;
use solana_program_runtime::loaded_programs::{BlockRelation, ForkGraph, ProgramCacheEntry};
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
        let feature_set = SVMFeatureSet::all_enabled();

        // The v1 program-runtime environment registers all standard syscalls so
        // real sBPF programs (SPL Token, Anchor, ...) verify and execute.
        let budget = SVMTransactionExecutionBudget::default();
        let loader = agave_syscalls::create_program_runtime_environment_v1(
            &feature_set,
            &budget,
            false, // reject_deployment_of_broken_elfs
            false, // debugging_features
        )
        .expect("create v1 program runtime environment");

        let processor = TransactionBatchProcessor::<TaoForkGraph>::new(
            slot,
            epoch,
            Arc::downgrade(&fork_graph),
            Some(Arc::new(loader)),
            None,
        );

        let bank = Self {
            db,
            processor,
            _fork_graph: fork_graph,
            slot,
            epoch,
            feature_set,
        };
        bank.install_sysvars();
        bank.register_system_builtin();
        bank.register_loader_builtins();
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
        self.put_sysvar(
            solana_sdk_ids::sysvar::rent::id(),
            &solana_rent::Rent::default(),
        );
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

    /// Register the BPF loader builtins so programs owned by them can execute.
    fn register_loader_builtins(&self) {
        let loaders = [
            (
                solana_sdk_ids::bpf_loader::id(),
                "solana_bpf_loader_program",
            ),
            (
                solana_sdk_ids::bpf_loader_deprecated::id(),
                "solana_bpf_loader_deprecated_program",
            ),
            (
                solana_sdk_ids::bpf_loader_upgradeable::id(),
                "solana_bpf_loader_upgradeable_program",
            ),
        ];
        for (id, name) in loaders {
            let account = AccountSharedData::from(Account {
                lamports: 1,
                data: name.as_bytes().to_vec(),
                owner: solana_sdk_ids::native_loader::id(),
                executable: true,
                rent_epoch: 0,
            });
            let _ = self.db.set(&id, &account);
            self.processor.add_builtin(
                id,
                ProgramCacheEntry::new_builtin(
                    0,
                    name.len(),
                    solana_bpf_loader_program::Entrypoint::vm,
                ),
            );
        }
    }

    /// Deploy an sBPF program (a `.so` ELF) at `program_id` as a non-upgradeable
    /// (`bpf_loader` v2) executable account. The SVM JIT-compiles it on first
    /// use. This is how SPL Token / Anchor programs get onto the chain.
    pub fn deploy_program(&self, program_id: &Pubkey, elf: &[u8]) -> Result<(), BankError> {
        let lamports = self.rent_exempt_minimum(elf.len()).max(1);
        let account = AccountSharedData::from(Account {
            lamports,
            data: elf.to_vec(),
            owner: solana_sdk_ids::bpf_loader::id(),
            executable: true,
            rent_epoch: 0,
        });
        self.db
            .set(program_id, &account)
            .map_err(|e| BankError::Storage(e.to_string()))
    }

    /// Rent-exempt minimum balance for an account of `space` bytes.
    pub fn rent_exempt_minimum(&self, space: usize) -> u64 {
        solana_rent::Rent::default().minimum_balance(space)
    }

    /// Latest account-set state root (delegated to the store).
    pub fn state_root(&self) -> Result<[u8; 32], BankError> {
        self.db
            .state_root()
            .map_err(|e| BankError::Storage(e.to_string()))
    }

    /// The underlying account store (for snapshotting / state queries).
    pub fn accounts(&self) -> &AccountsDb {
        &self.db
    }

    /// A cloned handle to the account store (for sharing with the RPC thread;
    /// RocksDB is internally synchronized).
    pub fn accounts_arc(&self) -> Arc<AccountsDb> {
        self.db.clone()
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
        tx.verify()
            .map_err(|e| BankError::BadSignature(e.to_string()))?;

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

        self.apply_result(
            &sanitized,
            output.processing_results.into_iter().next().unwrap(),
        )
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
                    self.db
                        .commit(writes)
                        .map_err(|e| BankError::Storage(e.to_string()))?;
                    Ok(TxOutcome {
                        succeeded: true,
                        fee,
                        error: None,
                    })
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
            Err(e) => Ok(TxOutcome {
                succeeded: false,
                fee: 0,
                error: Some(format!("{e:?}")),
            }),
        }
    }

    fn commit_rollback(
        &self,
        rollback: solana_svm::rollback_accounts::RollbackAccounts,
    ) -> Result<(), BankError> {
        // `iter()` yields the fee-payer (and nonce, if any) keyed accounts to
        // persist when a transaction is charged fees but not committed.
        let changes: Vec<(Pubkey, AccountSharedData)> = rollback
            .iter()
            .map(|(addr, acct)| (*addr, acct.clone()))
            .collect();
        if !changes.is_empty() {
            self.db
                .commit(changes)
                .map_err(|e| BankError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    /// Read an account's lamport balance (0 if absent).
    pub fn balance(&self, pubkey: &Pubkey) -> u64 {
        self.db
            .get(pubkey)
            .ok()
            .flatten()
            .map(|a| a.lamports())
            .unwrap_or(0)
    }

    /// Credit `lamports` to a System-owned account, creating it if absent.
    /// Used for the coinbase (block reward + collected fees → miner).
    fn credit(&self, pubkey: &Pubkey, lamports: u64) -> Result<(), BankError> {
        if lamports == 0 {
            return Ok(());
        }
        let mut account = self
            .db
            .get(pubkey)
            .map_err(|e| BankError::Storage(e.to_string()))?
            .unwrap_or_else(|| AccountSharedData::new(0, 0, &solana_sdk_ids::system_program::id()));
        account.set_lamports(account.lamports().saturating_add(lamports));
        self.db
            .set(pubkey, &account)
            .map_err(|e| BankError::Storage(e.to_string()))
    }

    /// Execute a block's transactions in order, pay the coinbase
    /// (`block_reward` newly minted + all collected fees) to `miner`, and return
    /// the resulting state. Transactions are applied sequentially so each sees
    /// the prior writes. Transactions that fail to even process (bad signature)
    /// are skipped.
    pub fn execute_block(
        &self,
        transactions: &[Transaction],
        blockhash: Hash,
        miner: &Pubkey,
        block_reward: u64,
    ) -> Result<BlockExecution, BankError> {
        let mut fees = 0u64;
        let mut outcomes = Vec::with_capacity(transactions.len());

        for tx in transactions {
            match self.execute_transaction(tx, blockhash.clone()) {
                Ok(outcome) => {
                    fees = fees.saturating_add(outcome.fee);
                    outcomes.push(outcome);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "transaction rejected before processing");
                    outcomes.push(TxOutcome {
                        succeeded: false,
                        fee: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        // Coinbase: newly minted reward plus recycled fees go to the miner.
        self.credit(miner, block_reward.saturating_add(fees))?;

        let state_root = self.state_root()?;
        Ok(BlockExecution {
            state_root,
            fees,
            reward: block_reward,
            outcomes,
        })
    }

    /// Credit lamports to an account out-of-band (e.g. a devnet faucet).
    /// Recorded in `state_root` only — callers wanting replayability must record
    /// the credit in the block.
    pub fn airdrop(&self, pubkey: &Pubkey, lamports: u64) -> Result<(), BankError> {
        self.credit(pubkey, lamports)
    }
}

/// Summary of executing one block.
#[derive(Debug, Clone)]
pub struct BlockExecution {
    /// Account-set state root after execution + coinbase.
    pub state_root: [u8; 32],
    /// Total fees collected from transactions.
    pub fees: u64,
    /// Block reward minted to the miner.
    pub reward: u64,
    /// Per-transaction outcomes, aligned 1:1 with the input transactions.
    pub outcomes: Vec<TxOutcome>,
}

impl BlockExecution {
    /// Number of transactions that committed successfully.
    pub fn executed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.succeeded).count()
    }

    /// Number of transactions that failed or were rejected.
    pub fn failed(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.succeeded).count()
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
        db.set(
            key,
            &AccountSharedData::new(lamports, 0, &solana_sdk_ids::system_program::id()),
        )
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
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

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
    fn execute_block_pays_coinbase_and_is_deterministic() {
        let payer = Keypair::new();
        let recipient = Keypair::new();
        let miner = Pubkey::new_unique();
        let blockhash = Hash::new_unique();
        let amount = 5_000_000u64;
        let reward = 1_000_000_000u64;
        let ix = solana_system_interface::instruction::transfer(
            &payer.pubkey(),
            &recipient.pubkey(),
            amount,
        );
        let tx =
            Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);

        let run = |tag: &str| {
            let db = tmp_db(tag);
            fund(&db, &payer.pubkey(), 1_000_000_000);
            let bank = Bank::new(db.clone(), 1);
            let exec = bank
                .execute_block(&[tx.clone()], blockhash, &miner, reward)
                .unwrap();
            (
                exec,
                bank.balance(&miner),
                bank.balance(&recipient.pubkey()),
            )
        };

        let (exec_a, miner_a, recip_a) = run("blk_a");
        assert_eq!(exec_a.executed(), 1);
        assert_eq!(exec_a.fees, LAMPORTS_PER_SIGNATURE);
        assert_eq!(recip_a, amount);
        // miner gets the newly-minted reward plus recycled fees.
        assert_eq!(miner_a, reward + LAMPORTS_PER_SIGNATURE);

        let (exec_b, miner_b, _) = run("blk_b");
        assert_eq!(
            exec_a.state_root, exec_b.state_root,
            "non-deterministic state root"
        );
        assert_eq!(miner_a, miner_b);
    }

    // ---- SPL Token: real sBPF program execution ----

    use solana_instruction::{AccountMeta, Instruction};
    use std::str::FromStr;

    /// The real, mainnet SPL Token program binary (v3.5.0), deployed on-chain.
    const SPL_TOKEN_ELF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../programs/spl_token.so"
    ));

    fn spl_token_id() -> Pubkey {
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
    }

    // Minimal SPL Token instruction encoders (avoid a spl-token crate dep that
    // would fight our locked solana versions). Tags per spl-token 3.x.
    fn ix_initialize_mint2(
        token: &Pubkey,
        mint: &Pubkey,
        authority: &Pubkey,
        decimals: u8,
    ) -> Instruction {
        let mut data = vec![20u8, decimals]; // InitializeMint2
        data.extend_from_slice(authority.as_ref());
        data.push(0); // freeze authority COption::None
        Instruction {
            program_id: *token,
            accounts: vec![AccountMeta::new(*mint, false)],
            data,
        }
    }

    fn ix_initialize_account3(
        token: &Pubkey,
        account: &Pubkey,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> Instruction {
        let mut data = vec![18u8]; // InitializeAccount3
        data.extend_from_slice(owner.as_ref());
        Instruction {
            program_id: *token,
            accounts: vec![
                AccountMeta::new(*account, false),
                AccountMeta::new_readonly(*mint, false),
            ],
            data,
        }
    }

    fn ix_mint_to(
        token: &Pubkey,
        mint: &Pubkey,
        dest: &Pubkey,
        authority: &Pubkey,
        amount: u64,
    ) -> Instruction {
        let mut data = vec![7u8]; // MintTo
        data.extend_from_slice(&amount.to_le_bytes());
        Instruction {
            program_id: *token,
            accounts: vec![
                AccountMeta::new(*mint, false),
                AccountMeta::new(*dest, false),
                AccountMeta::new_readonly(*authority, true),
            ],
            data,
        }
    }

    #[test]
    fn deploys_and_runs_spl_token() {
        let db = tmp_db("spl");
        let payer = Keypair::new();
        let mint = Keypair::new();
        let token_account = Keypair::new();
        fund(&db, &payer.pubkey(), 1_000_000_000);

        let bank = Bank::new(db.clone(), 1);
        let token = spl_token_id();
        bank.deploy_program(&token, SPL_TOKEN_ELF).unwrap();

        let blockhash = Hash::new_unique();
        let mint_rent = bank.rent_exempt_minimum(82);
        let acct_rent = bank.rent_exempt_minimum(165);

        // 1) Create + initialize the mint (decimals = 0).
        let tx1 = Transaction::new_signed_with_payer(
            &[
                solana_system_interface::instruction::create_account(
                    &payer.pubkey(),
                    &mint.pubkey(),
                    mint_rent,
                    82,
                    &token,
                ),
                ix_initialize_mint2(&token, &mint.pubkey(), &payer.pubkey(), 0),
            ],
            Some(&payer.pubkey()),
            &[&payer, &mint],
            blockhash,
        );
        let o1 = bank.execute_transaction(&tx1, blockhash).unwrap();
        assert!(o1.succeeded, "create+init mint failed: {:?}", o1.error);

        // 2) Create + initialize a token account owned by the payer.
        let tx2 = Transaction::new_signed_with_payer(
            &[
                solana_system_interface::instruction::create_account(
                    &payer.pubkey(),
                    &token_account.pubkey(),
                    acct_rent,
                    165,
                    &token,
                ),
                ix_initialize_account3(
                    &token,
                    &token_account.pubkey(),
                    &mint.pubkey(),
                    &payer.pubkey(),
                ),
            ],
            Some(&payer.pubkey()),
            &[&payer, &token_account],
            blockhash,
        );
        let o2 = bank.execute_transaction(&tx2, blockhash).unwrap();
        assert!(
            o2.succeeded,
            "create+init token account failed: {:?}",
            o2.error
        );

        // 3) Mint 1_000_000 tokens to the token account.
        let amount = 1_000_000u64;
        let tx3 = Transaction::new_signed_with_payer(
            &[ix_mint_to(
                &token,
                &mint.pubkey(),
                &token_account.pubkey(),
                &payer.pubkey(),
                amount,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            blockhash,
        );
        let o3 = bank.execute_transaction(&tx3, blockhash).unwrap();
        assert!(o3.succeeded, "mint_to failed: {:?}", o3.error);

        // Verify: SPL token account `amount` field is at byte offset 64..72.
        let acct = db.get(&token_account.pubkey()).unwrap().unwrap();
        let data = acct.data();
        assert_eq!(data.len(), 165, "token account size");
        let balance = u64::from_le_bytes(data[64..72].try_into().unwrap());
        assert_eq!(balance, amount, "SPL token balance after mint_to");
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
