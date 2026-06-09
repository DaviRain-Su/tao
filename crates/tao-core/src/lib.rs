//! `tao-core` — shared foundations for the Tao chain.
//!
//! This crate holds cross-cutting pieces every other crate depends on:
//! - [`error`]: the unified error type and `Result` alias.
//! - [`config`]: node runtime configuration (data dir, ports, network).
//! - [`logging`]: `tracing` subscriber setup.
//! - [`genesis`]: the genesis configuration format.
//!
//! It also re-exports the Solana-compatible primitive types we standardize on
//! ([`Pubkey`], [`Hash`]) so the rest of the workspace shares one source of truth.

pub mod config;
pub mod error;
pub mod genesis;
pub mod logging;

pub use error::{Result, TaoError};

/// Ed25519 public key / account address — Solana-compatible (32 bytes, base58).
pub use solana_pubkey::Pubkey;

/// 32-byte hash — Solana-compatible.
pub use solana_hash::Hash;

/// Human-readable name of this software.
pub const CLIENT_NAME: &str = "tao";

/// Crate version, sourced from Cargo at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
