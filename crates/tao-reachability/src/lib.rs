//! `tao-reachability` — an efficient blockDAG reachability index.
//!
//! Ported from rusty-kaspa (github.com/kaspanet/rusty-kaspa, ISC License,
//! `consensus/src/processes/reachability/` and
//! `consensus/src/model/stores/reachability.rs`). The reachability algorithm in
//! rusty-kaspa is written against storage *traits* (no RocksDB in the algorithm
//! itself), but it ships inside the RocksDB-bound `kaspa-consensus` crate — so we
//! port it (permitted under ISC) rather than depend on the whole node.
//!
//! It replaces O(n) `past`-set scans with **interval-tree reachability**: each
//! block gets a `u64` interval that strictly contains its tree-subtree's
//! intervals, so `is_chain_ancestor_of` is O(1) interval containment and
//! `is_dag_ancestor_of` is O(log n) via a per-block future-covering set.
//!
//! The algorithm operates on an opaque 32-byte block id ([`Hash`], a small local
//! newtype here — `kaspa-hashes` pulls a WASM dep tree that doesn't build on this
//! toolchain, and the index only needs an opaque id). Storage-layer types from
//! `kaspa-database` are replaced by the light local aliases below. Integrating
//! with the live chain is a trivial `[u8; 32]` ↔ `Hash` conversion.
//!
//! **Layering status:** this commit ports the foundation + storage layer
//! (`Interval`, the store traits, and `MemoryReachabilityStore`). The query +
//! insertion + reindex algorithm (`inquirer`/`tree`/`reindex`) is ported on top
//! of these traits next.

mod extensions;
pub mod inquirer;
mod interval;
mod reindex;
mod store;
#[cfg(test)]
mod tests;
mod tree;

pub use interval::Interval;
pub use store::{MemoryReachabilityStore, ReachabilityStore, ReachabilityStoreReader};

use std::collections::{HashMap, HashSet};
use std::fmt;

/// Performance / safety constants (rusty-kaspa `core/config/constants.rs`).
pub mod constants {
    pub mod perf {
        /// How far behind the tip the reindex root trails.
        pub const DEFAULT_REINDEX_DEPTH: u64 = 100;
        /// Slack reserved per chain block below the reindex root (2^14).
        pub const DEFAULT_REINDEX_SLACK: u64 = 1 << 14;
    }
}
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// An opaque 32-byte block id. (Production maps the chain's real block hash to
/// this via `Hash::from_bytes(header.id())`.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash([u8; 32]);

impl Hash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Hash(bytes)
    }

    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Is this the `NONE` sentinel (no parent)?
    pub fn is_none(&self) -> bool {
        *self == blockhash::NONE
    }
}

impl From<u64> for Hash {
    fn from(v: u64) -> Self {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&v.to_le_bytes());
        Hash(b)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "..")
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

/// An ordered, shareable list of block hashes (tree children / future-covering set).
pub type BlockHashes = Arc<Vec<Hash>>;
/// A map keyed by block hash.
pub type BlockHashMap<V> = HashMap<Hash, V>;
/// A set of block hashes.
pub type BlockHashSet = HashSet<Hash>;

/// Sentinel block hashes (the reachability tree root and the "no block" marker).
pub mod blockhash {
    use super::Hash;
    /// The reachability tree root (a virtual block above genesis).
    pub const ORIGIN: Hash = Hash::from_bytes([0xff; 32]);
    /// The "no parent" sentinel.
    pub const NONE: Hash = Hash::from_bytes([0; 32]);
}

/// Storage errors surfaced by the reachability stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("reachability key not found: {0}")]
    KeyNotFound(Hash),
    #[error("reachability hash already exists: {0}")]
    HashAlreadyExists(Hash),
}

impl StoreError {
    pub fn is_key_not_found(&self) -> bool {
        matches!(self, StoreError::KeyNotFound(_))
    }
    pub fn is_already_exists(&self) -> bool {
        matches!(self, StoreError::HashAlreadyExists(_))
    }
}

/// Result alias for store operations.
pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// Errors from the reachability algorithm.
#[derive(Debug, thiserror::Error)]
pub enum ReachabilityError {
    #[error("data store error")]
    StoreError(#[from] StoreError),
    #[error("data overflow error: {0}")]
    DataOverflow(String),
    #[error("data inconsistency error")]
    DataInconsistency,
    #[error("query is inconsistent")]
    BadQuery,
}

impl ReachabilityError {
    pub fn is_key_not_found(&self) -> bool {
        matches!(self, ReachabilityError::StoreError(err) if err.is_key_not_found())
    }
}

/// Result alias for the reachability algorithm.
pub type Result<T> = std::result::Result<T, ReachabilityError>;
