//! `tao-database` — durable persistence for the Tao chain.
//!
//! M1 ships a simple, dependency-free **append-only block log**: every accepted
//! block (main-chain and side-branch) is appended as a length-prefixed record.
//! On startup the node replays the log through the consensus fork-choice to
//! rebuild the chain state deterministically.
//!
//! A columnar RocksDB store (for the account state DB and indexed lookups)
//! lands in **M2**, where mutable account state makes it worth the dependency.

mod accounts_db;
mod block_log;

pub use accounts_db::AccountsDb;
pub use block_log::BlockLog;
