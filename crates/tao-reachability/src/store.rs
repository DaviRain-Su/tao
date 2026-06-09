// Ported from rusty-kaspa (github.com/kaspanet/rusty-kaspa), ISC License,
// consensus/src/model/stores/reachability.rs — the store traits and the
// (test-intended) in-memory store only. The RocksDB and staging stores are
// omitted; storage-layer types (BlockHashMap, StoreError, DbKey, ...) are
// replaced by the light local aliases in `crate`.

use std::collections::hash_map::Entry::Vacant;
use std::sync::Arc;

use crate::interval::Interval;
use crate::{blockhash, BlockHashMap, BlockHashes, Hash, StoreError};

/// Reader API for [`ReachabilityStore`].
pub trait ReachabilityStoreReader {
    fn has(&self, hash: Hash) -> Result<bool, StoreError>;
    fn get_interval(&self, hash: Hash) -> Result<Interval, StoreError>;
    /// The reachability *tree* parent of `hash`.
    fn get_parent(&self, hash: Hash) -> Result<Hash, StoreError>;
    /// The reachability *tree* children of `hash`.
    fn get_children(&self, hash: Hash) -> Result<BlockHashes, StoreError>;
    fn get_future_covering_set(&self, hash: Hash) -> Result<BlockHashes, StoreError>;
    /// Number of entries (tests only).
    fn count(&self) -> Result<usize, StoreError>;
}

/// Write API. All writes are `&mut` since reachability is not append-only.
pub trait ReachabilityStore: ReachabilityStoreReader {
    fn init(&mut self, origin: Hash, capacity: Interval) -> Result<(), StoreError>;
    fn insert(
        &mut self,
        hash: Hash,
        parent: Hash,
        interval: Interval,
        height: u64,
    ) -> Result<(), StoreError>;
    fn set_interval(&mut self, hash: Hash, interval: Interval) -> Result<(), StoreError>;
    fn append_child(&mut self, hash: Hash, child: Hash) -> Result<(), StoreError>;
    fn insert_future_covering_item(
        &mut self,
        hash: Hash,
        fci: Hash,
        insertion_index: usize,
    ) -> Result<(), StoreError>;
    fn set_parent(&mut self, hash: Hash, new_parent: Hash) -> Result<(), StoreError>;
    fn replace_child(
        &mut self,
        hash: Hash,
        replaced_hash: Hash,
        replaced_index: usize,
        replace_with: &[Hash],
    ) -> Result<(), StoreError>;
    fn replace_future_covering_item(
        &mut self,
        hash: Hash,
        replaced_hash: Hash,
        replaced_index: usize,
        replace_with: &[Hash],
    ) -> Result<(), StoreError>;
    fn delete(&mut self, hash: Hash) -> Result<(), StoreError>;
    fn get_height(&self, hash: Hash) -> Result<u64, StoreError>;
    fn set_reindex_root(&mut self, root: Hash) -> Result<(), StoreError>;
    fn get_reindex_root(&self) -> Result<Hash, StoreError>;
}

/// In-memory reachability data, grouping tree children and the future covering
/// set inline (the DB store decomposes these into separate keyed sets).
#[derive(Clone)]
struct MemoryReachabilityData {
    children: BlockHashes,
    parent: Hash,
    interval: Interval,
    height: u64,
    future_covering_set: BlockHashes,
}

impl MemoryReachabilityData {
    fn new(parent: Hash, interval: Interval, height: u64) -> Self {
        Self {
            children: Arc::new(vec![]),
            parent,
            interval,
            height,
            future_covering_set: Arc::new(vec![]),
        }
    }
}

/// An in-memory [`ReachabilityStore`] — the reference/test store and the basis
/// for the ported algorithm. A persistent (RocksDB) store can implement the same
/// traits later.
pub struct MemoryReachabilityStore {
    map: BlockHashMap<MemoryReachabilityData>,
    reindex_root: Option<Hash>,
}

impl Default for MemoryReachabilityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryReachabilityStore {
    pub fn new() -> Self {
        Self {
            map: BlockHashMap::new(),
            reindex_root: None,
        }
    }

    fn get_data_mut(&mut self, hash: Hash) -> Result<&mut MemoryReachabilityData, StoreError> {
        self.map.get_mut(&hash).ok_or(StoreError::KeyNotFound(hash))
    }

    fn get_data(&self, hash: Hash) -> Result<&MemoryReachabilityData, StoreError> {
        self.map.get(&hash).ok_or(StoreError::KeyNotFound(hash))
    }
}

impl ReachabilityStore for MemoryReachabilityStore {
    fn init(&mut self, origin: Hash, capacity: Interval) -> Result<(), StoreError> {
        self.insert(origin, blockhash::NONE, capacity, 0)?;
        self.set_reindex_root(origin)?;
        Ok(())
    }

    fn insert(
        &mut self,
        hash: Hash,
        parent: Hash,
        interval: Interval,
        height: u64,
    ) -> Result<(), StoreError> {
        if let Vacant(e) = self.map.entry(hash) {
            e.insert(MemoryReachabilityData::new(parent, interval, height));
            Ok(())
        } else {
            Err(StoreError::HashAlreadyExists(hash))
        }
    }

    fn set_interval(&mut self, hash: Hash, interval: Interval) -> Result<(), StoreError> {
        self.get_data_mut(hash)?.interval = interval;
        Ok(())
    }

    fn append_child(&mut self, hash: Hash, child: Hash) -> Result<(), StoreError> {
        let data = self.get_data_mut(hash)?;
        Arc::make_mut(&mut data.children).push(child);
        Ok(())
    }

    fn insert_future_covering_item(
        &mut self,
        hash: Hash,
        fci: Hash,
        insertion_index: usize,
    ) -> Result<(), StoreError> {
        let data = self.get_data_mut(hash)?;
        Arc::make_mut(&mut data.future_covering_set).insert(insertion_index, fci);
        Ok(())
    }

    fn set_parent(&mut self, hash: Hash, new_parent: Hash) -> Result<(), StoreError> {
        self.get_data_mut(hash)?.parent = new_parent;
        Ok(())
    }

    fn replace_child(
        &mut self,
        hash: Hash,
        replaced_hash: Hash,
        replaced_index: usize,
        replace_with: &[Hash],
    ) -> Result<(), StoreError> {
        let data = self.get_data_mut(hash)?;
        let removed: Vec<Hash> = Arc::make_mut(&mut data.children)
            .splice(
                replaced_index..replaced_index + 1,
                replace_with.iter().copied(),
            )
            .collect();
        debug_assert_eq!(removed.len(), 1);
        debug_assert_eq!(replaced_hash, removed[0]);
        Ok(())
    }

    fn replace_future_covering_item(
        &mut self,
        hash: Hash,
        replaced_hash: Hash,
        replaced_index: usize,
        replace_with: &[Hash],
    ) -> Result<(), StoreError> {
        let data = self.get_data_mut(hash)?;
        let removed: Vec<Hash> = Arc::make_mut(&mut data.future_covering_set)
            .splice(
                replaced_index..replaced_index + 1,
                replace_with.iter().copied(),
            )
            .collect();
        debug_assert_eq!(removed.len(), 1);
        debug_assert_eq!(replaced_hash, removed[0]);
        Ok(())
    }

    fn delete(&mut self, hash: Hash) -> Result<(), StoreError> {
        self.map.remove(&hash);
        Ok(())
    }

    fn get_height(&self, hash: Hash) -> Result<u64, StoreError> {
        Ok(self.get_data(hash)?.height)
    }

    fn set_reindex_root(&mut self, root: Hash) -> Result<(), StoreError> {
        self.reindex_root = Some(root);
        Ok(())
    }

    fn get_reindex_root(&self) -> Result<Hash, StoreError> {
        self.reindex_root
            .ok_or(StoreError::KeyNotFound(blockhash::NONE))
    }
}

impl ReachabilityStoreReader for MemoryReachabilityStore {
    fn has(&self, hash: Hash) -> Result<bool, StoreError> {
        Ok(self.map.contains_key(&hash))
    }

    fn get_interval(&self, hash: Hash) -> Result<Interval, StoreError> {
        Ok(self.get_data(hash)?.interval)
    }

    fn get_parent(&self, hash: Hash) -> Result<Hash, StoreError> {
        Ok(self.get_data(hash)?.parent)
    }

    fn get_children(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        Ok(Arc::clone(&self.get_data(hash)?.children))
    }

    fn get_future_covering_set(&self, hash: Hash) -> Result<BlockHashes, StoreError> {
        Ok(Arc::clone(&self.get_data(hash)?.future_covering_set))
    }

    fn count(&self) -> Result<usize, StoreError> {
        Ok(self.map.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_basics() {
        let mut store: Box<dyn ReachabilityStore> = Box::new(MemoryReachabilityStore::new());
        let (hash, parent) = (7.into(), 15.into());
        store.insert(hash, parent, Interval::maximal(), 5).unwrap();
        store.append_child(hash, 31.into()).unwrap();
        assert_eq!(store.get_height(hash).unwrap(), 5);
        assert_eq!(store.get_children(hash).unwrap().len(), 1);
        assert_eq!(store.get_parent(hash).unwrap(), parent);
        store.get_interval(7.into()).unwrap();
    }

    #[test]
    fn init_sets_origin_and_reindex_root() {
        let mut store = MemoryReachabilityStore::new();
        let origin = blockhash::ORIGIN;
        store.init(origin, Interval::maximal()).unwrap();
        assert!(store.has(origin).unwrap());
        assert_eq!(store.get_reindex_root().unwrap(), origin);
        assert_eq!(store.get_interval(origin).unwrap(), Interval::maximal());
    }
}
