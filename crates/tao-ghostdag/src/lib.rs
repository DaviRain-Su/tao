//! `tao-ghostdag` — the GHOSTDAG protocol on top of [`tao_reachability`].
//!
//! Ported from rusty-kaspa (github.com/kaspanet/rusty-kaspa, ISC,
//! `consensus/src/processes/ghostdag/` + `model/stores/ghostdag.rs`). GHOSTDAG
//! classifies each block **blue** or **red** via the greedy k-cluster rule,
//! using the reachability index for `is_dag_ancestor_of` anticone tests, and
//! tracks blue score / blue work for the heaviest-chain fork choice.
//!
//! Adaptations from the rusty-kaspa source:
//! - `BlueWorkType` (a `kaspa-math` Uint192) → `u128`; per-block work comes from
//!   a small [`WorkStore`] trait instead of `HeaderStoreReader::get_bits` +
//!   `calc_work` (wire `work_for_target(header.target)` in production).
//! - `kaspa_utils::refs::Refs` → `Arc<GhostdagData>` (one clone of the new
//!   block's data per candidate — fine for this layer).
//! - The `ReachabilityService` is a minimal local trait (only `is_dag_ancestor_of`
//!   / `is_chain_ancestor_of` are needed) over the reachability store.
//! - Storage uses interior-mutability in-memory stores (the reference impls);
//!   a persistent store implements the same traits later.

mod engine;
mod protocol;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::sync::{Arc, RwLock};

pub use engine::DagEngine;
pub use protocol::{GhostdagData, GhostdagManager, SortableBlock};
pub use tao_reachability::{blockhash, BlockHashes, Hash, ReachabilityStoreReader, StoreError};

use tao_reachability::{inquirer, BlockHashMap};

/// The k-cluster bound type (matches rusty-kaspa).
pub type KType = u16;
/// Per-block blue-anticone-size map (shared).
pub type HashKTypeMap = Arc<BlockHashMap<KType>>;
/// Accumulated work along the selected (blue) chain.
pub type BlueWork = u128;

// ---------------------------------------------------------------------------
// Reachability service (minimal — GHOSTDAG only needs ancestor queries).
// ---------------------------------------------------------------------------

/// Read-only reachability queries used by GHOSTDAG.
pub trait ReachabilityService {
    fn is_dag_ancestor_of(&self, this: Hash, queried: Hash) -> bool;
    fn is_chain_ancestor_of(&self, this: Hash, queried: Hash) -> bool;
}

/// A reachability service over a shared, lockable reachability store.
#[derive(Clone)]
pub struct MtReachabilityService<S: ReachabilityStoreReader> {
    store: Arc<RwLock<S>>,
}

impl<S: ReachabilityStoreReader> MtReachabilityService<S> {
    pub fn new(store: Arc<RwLock<S>>) -> Self {
        Self { store }
    }
}

impl<S: ReachabilityStoreReader> ReachabilityService for MtReachabilityService<S> {
    fn is_dag_ancestor_of(&self, this: Hash, queried: Hash) -> bool {
        inquirer::is_dag_ancestor_of(&*self.store.read().unwrap(), this, queried).unwrap()
    }
    fn is_chain_ancestor_of(&self, this: Hash, queried: Hash) -> bool {
        inquirer::is_chain_ancestor_of(&*self.store.read().unwrap(), this, queried).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Per-block work (replaces HeaderStoreReader::get_bits + calc_work).
// ---------------------------------------------------------------------------

/// Supplies each block's individual PoW work (production: `work_for_target`).
pub trait WorkStore {
    fn get_work(&self, hash: Hash) -> BlueWork;
}

/// A trivial work store assigning unit work to every block (blue_work == blue_score).
#[derive(Clone, Copy, Default)]
pub struct UnitWork;

impl WorkStore for UnitWork {
    fn get_work(&self, _hash: Hash) -> BlueWork {
        1
    }
}

// ---------------------------------------------------------------------------
// Relations store (block → parents).
// ---------------------------------------------------------------------------

pub trait RelationsStoreReader {
    fn get_parents(&self, hash: Hash) -> Result<BlockHashes, StoreError>;
}

impl<T: RelationsStoreReader + ?Sized> RelationsStoreReader for Arc<T> {
    fn get_parents(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        (**self).get_parents(hash)
    }
}

/// In-memory relations store (interior mutability for `&self` insert).
#[derive(Default)]
pub struct MemoryRelationsStore {
    map: RefCell<BlockHashMap<BlockHashes>>,
}

impl MemoryRelationsStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, hash: Hash, parents: BlockHashes) {
        self.map.borrow_mut().insert(hash, parents);
    }
}

impl RelationsStoreReader for MemoryRelationsStore {
    fn get_parents(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        self.map.borrow().get(&hash).cloned().ok_or(StoreError::KeyNotFound(hash))
    }
}

// ---------------------------------------------------------------------------
// GHOSTDAG store (block → GhostdagData).
// ---------------------------------------------------------------------------

pub trait GhostdagStoreReader {
    fn get_blue_score(&self, hash: Hash) -> Result<u64, StoreError>;
    fn get_blue_work(&self, hash: Hash) -> Result<BlueWork, StoreError>;
    fn get_selected_parent(&self, hash: Hash) -> Result<Hash, StoreError>;
    fn get_mergeset_blues(&self, hash: Hash) -> Result<BlockHashes, StoreError>;
    fn get_blues_anticone_sizes(&self, hash: Hash) -> Result<HashKTypeMap, StoreError>;
    fn get_data(&self, hash: Hash) -> Result<Arc<GhostdagData>, StoreError>;
    fn has(&self, hash: Hash) -> Result<bool, StoreError>;
}

pub trait GhostdagStore: GhostdagStoreReader {
    fn insert(&self, hash: Hash, data: Arc<GhostdagData>) -> Result<(), StoreError>;
}

/// In-memory GHOSTDAG store.
#[derive(Default)]
pub struct MemoryGhostdagStore {
    map: RefCell<BlockHashMap<Arc<GhostdagData>>>,
}

impl MemoryGhostdagStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GhostdagStore for MemoryGhostdagStore {
    fn insert(&self, hash: Hash, data: Arc<GhostdagData>) -> Result<(), StoreError> {
        self.map.borrow_mut().insert(hash, data);
        Ok(())
    }
}

impl GhostdagStoreReader for MemoryGhostdagStore {
    fn get_blue_score(&self, hash: Hash) -> Result<u64, StoreError> {
        Ok(self.get_data(hash)?.blue_score)
    }
    fn get_blue_work(&self, hash: Hash) -> Result<BlueWork, StoreError> {
        Ok(self.get_data(hash)?.blue_work)
    }
    fn get_selected_parent(&self, hash: Hash) -> Result<Hash, StoreError> {
        Ok(self.get_data(hash)?.selected_parent)
    }
    fn get_mergeset_blues(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        Ok(self.get_data(hash)?.mergeset_blues.clone())
    }
    fn get_blues_anticone_sizes(&self, hash: Hash) -> Result<HashKTypeMap, StoreError> {
        Ok(self.get_data(hash)?.blues_anticone_sizes.clone())
    }
    fn get_data(&self, hash: Hash) -> Result<Arc<GhostdagData>, StoreError> {
        self.map.borrow().get(&hash).cloned().ok_or(StoreError::KeyNotFound(hash))
    }
    fn has(&self, hash: Hash) -> Result<bool, StoreError> {
        Ok(self.map.borrow().contains_key(&hash))
    }
}

impl<T: GhostdagStoreReader + ?Sized> GhostdagStoreReader for Arc<T> {
    fn get_blue_score(&self, hash: Hash) -> Result<u64, StoreError> {
        (**self).get_blue_score(hash)
    }
    fn get_blue_work(&self, hash: Hash) -> Result<BlueWork, StoreError> {
        (**self).get_blue_work(hash)
    }
    fn get_selected_parent(&self, hash: Hash) -> Result<Hash, StoreError> {
        (**self).get_selected_parent(hash)
    }
    fn get_mergeset_blues(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        (**self).get_mergeset_blues(hash)
    }
    fn get_blues_anticone_sizes(&self, hash: Hash) -> Result<HashKTypeMap, StoreError> {
        (**self).get_blues_anticone_sizes(hash)
    }
    fn get_data(&self, hash: Hash) -> Result<Arc<GhostdagData>, StoreError> {
        (**self).get_data(hash)
    }
    fn has(&self, hash: Hash) -> Result<bool, StoreError> {
        (**self).has(hash)
    }
}

/// Is `hash` the reachability/relations ORIGIN sentinel?
pub fn is_origin(hash: Hash) -> bool {
    hash == blockhash::ORIGIN
}
