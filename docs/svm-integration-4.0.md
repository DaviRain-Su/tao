# solana-svm 4.0.0 Integration Notes (M2b reference)

Distilled from a deep dive into the Agave `v4.0.0` source (`svm/tests/mock_bank.rs`,
`svm/tests/integration_test.rs`, `svm-callback/src/lib.rs`, `svm/src/transaction_processor.rs`).
The `paytube` example was **removed** at v4.0.0; the in-tree reference is now the SVM
integration tests. `docs.rs/solana-svm/4.0.0` 404s for several types — source is authoritative.

## ⚠️ Version skew is the #1 footgun

`solana-svm 4.0.0` itself is 4.x, but it depends on the **3.x** lines of several crates.
Mixing 4.x of those causes *two* copies of `AccountSharedData`/`Transaction` and type
mismatches at the SVM boundary. Verify with `cargo tree -d` (no solana crate should appear twice).

| Crate | Version | Notes |
|---|---|---|
| solana-svm | 4.0.0 | enable `dev-context-only-utils` for `new()` + `from_transaction_for_tests` |
| solana-svm-callback / -feature-set / -transaction / -type-overrides | 4.0.0 | |
| solana-program-runtime / -compute-budget / -transaction-context | 4.0.0 | |
| solana-system-program | 4.0.0 | `system_processor::Entrypoint::vm`, `id()` |
| solana-sbpf | =0.14.4 | must match SVM's pin exactly |
| solana-pubkey | 4.1 | 4.x |
| solana-hash | 4.2 | 4.x |
| **solana-account** | **3.4** | **NOT 4.x** — `AccountSharedData`, `ReadableAccount` |
| **solana-transaction** | **3.1** | `Transaction`, `SanitizedTransaction` |
| **solana-message** | **3.1** | |
| solana-fee-structure | 3.0 | `FeeDetails` |
| solana-rent | 3.0 | `Rent` |
| solana-clock | 3.0 | `Clock`, `Slot`, `Epoch` |
| solana-sdk-ids | 3.1 | `native_loader::id`, `system_program::id` |
| solana-transaction-error | 3.1 | |
| solana-keypair / -signer / -system-interface | 3.x | tx building |

## 1. `TransactionProcessingCallback` (4.0 — much smaller than 2.x)

Only **one required method**; must also `impl InvokeContextCallback` (empty ok). No
`account_matches_owners` / `add_builtin_account` anymore (the spec doc is stale).

```rust
impl InvokeContextCallback for DbCallback<'_> {}
impl TransactionProcessingCallback for DbCallback<'_> {
    // NOTE: returns a TUPLE with slot in 4.x
    fn get_account_shared_data(&self, k: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        self.db.get(k).map(|a| (a, 0))
    }
}
```

## 2. Processor + ForkGraph + builtins

- `ForkGraph` from `solana_program_runtime::loaded_programs` — single-fork stub maps
  `a.cmp(&b)` → Ancestor/Equal/Descendant.
- `TransactionBatchProcessor::new(slot, epoch, Arc::downgrade(&fork_graph), Some(Arc::new(loader_v1)), None)`
  is gated behind `dev-context-only-utils` (easiest path). Else `new_uninitialized` +
  `global_program_cache.write().set_fork_graph(...)`.
- Register System builtin: insert a `native_loader`-owned `executable` account at
  `solana_system_program::id()`, then
  `processor.add_builtin(id, ProgramCacheEntry::new_builtin(0, name.len(), system_processor::Entrypoint::vm))`.
- v1 loader (syscalls): copy `create_custom_loader()` from `mock_bank.rs:358`.

## 3. Sysvars + environment

- Put serialized `Clock`/`Rent`/`EpochSchedule` accounts into the store (keyed by `Sysvar::id()`),
  then `processor.fill_missing_sysvar_cache_entries(&callback)`.
- `TransactionProcessingEnvironment { blockhash, blockhash_lamports_per_signature, epoch_total_stake,
  feature_set: SVMFeatureSet::all_enabled(), program_runtime_environments_for_execution/_deployment:
  processor.get_environments_for_epoch(epoch), rent: Rent::default() }`.
- No `fee_per_signature` / `rent_collector` fields in 4.0. Fees are host-supplied (see §4).

## 4. Execute

```rust
processor.load_and_execute_sanitized_transactions(
    &callback, &[sanitized], check_results, &env, &config) -> LoadAndExecuteSanitizedTransactionsOutput
```
- `check_results: Vec<TransactionCheckResult>` MUST be same length as txs. Each =
  `Ok(CheckedTransactionDetails::new(nonce_addr, limits))` where `limits =
  ComputeBudgetLimits::get_compute_budget_and_limits(bytes, FeeDetails::new(sig_count*LPS, prio), simd_flag)`.
  Blockhash-age / nonce checks are **the host's job** before this call; put `Err(TransactionError::…)`
  at an index to mark it dropped.
- `SanitizedTransaction::from_transaction_for_tests(tx)` (dev-context) or `try_create(versioned, hash,
  Some(is_vote), &SimpleAddressLoader::Disabled, &empty_reserved_keys)`.
- `TransactionProcessingConfig { recording_config: ExecutionRecordingConfig::new_single_setting(true),
  ..Default::default() }`. New 4.x fields: `drop_on_failure`, `all_or_nothing`. No `compute_budget` field.

## 5. Apply results → AccountsDb

`processing_results[i]: Result<ProcessedTransaction>`:
- `Ok(Executed(ex))` + `ex.was_successful()` → commit writable `ex.loaded_transaction.accounts`
  (`Vec<(Pubkey, AccountSharedData)>`); lamports==0 ⇒ store default (deallocated).
- `Ok(Executed(ex))` program-failed → commit `ex.loaded_transaction.rollback_accounts` (fee/nonce only).
- `Ok(FeesOnly(fo))` → commit `fo.rollback_accounts`.
- `Err(_)` → dropped, commit nothing.
Fees via `ProcessedTransaction::fee_details()`. Both `Executed`/`FeesOnly` are `Box`ed.

## 6. Gotchas
- `get_account_shared_data` returns `Option<(AccountSharedData, Slot)>` (tuple).
- `feature_set` is `SVMFeatureSet` (flat bools), not runtime `FeatureSet`.
- per-tx budget lives in `check_results`, not config/env.
- `ExecutedTransaction`/`FeesOnlyTransaction` are boxed.
- config field is `check_program_deployment_slot` (not `…modification_slot`).
- `get_compute_budget_and_limits` 3rd bool arg: `simd_0268_active` (rc) vs `raise_cpi_nesting_limit_to_8`
  (published test) — pass the matching `SVMFeatureSet` bool; check the compiled signature.

## Reference (tag v4.0.0)
- Callback: `svm-callback/src/lib.rs`
- Processor/config/env: `svm/src/transaction_processor.rs` (new:256, load_and_execute:364, add_builtin:1163, fill_missing_sysvar:1141)
- Results: `svm/src/transaction_processing_result.rs`, `transaction_execution_result.rs`, `account_loader.rs`
- **Usage example: `svm/tests/mock_bank.rs` + `svm/tests/integration_test.rs`** (setup 121-194, apply 210-254, check_results 533-567)
