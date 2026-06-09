//! `tao-runtime` — the execution layer.
//!
//! Wraps the account store ([`tao_database::AccountsDb`]) with the machinery a
//! block needs to execute Solana transactions:
//! - [`blockhash_queue`]: recent-blockhash tracking for replay protection.
//! - [`genesis`]: apply genesis allocations into the account store.
//! - **Bank + SVM execution** (`solana-svm`): next step (M2b) — see
//!   `docs/svm-integration-4.0.md`.
//!
//! Scaffold for milestone **M2**.

pub mod bank;
pub mod blockhash_queue;
pub mod genesis;

pub use bank::{Bank, BankError, BlockExecution, TxOutcome, LAMPORTS_PER_SIGNATURE};
pub use blockhash_queue::BlockhashQueue;
pub use genesis::{load_allocations, GenesisLoad};
