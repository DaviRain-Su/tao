//! `DagEngine` — a usable blockDAG engine combining the ported reachability
//! index and GHOSTDAG. It accepts multi-parent blocks, classifies them
//! blue/red, tracks the heaviest tip (max blue work), and produces the
//! deterministic **total order** (GHOSTDAG linearization) — the order in which
//! transactions would be fed to the SVM (the "Case B" linearization that keeps
//! Solana-SVM compatibility under a blockDAG).
//!
//! This replaces the `tao-dag` prototype's O(n) past-set scans with Kaspa's
//! O(1)/O(log n) reachability index + the production GHOSTDAG.

use std::cell::RefCell;
use std::sync::{Arc, RwLock};

use tao_reachability::{blockhash, inquirer, BlockHashes, Hash, MemoryReachabilityStore};

use crate::{
    GhostdagData, GhostdagManager, GhostdagStore, GhostdagStoreReader, KType, MemoryGhostdagStore,
    MemoryRelationsStore, MtReachabilityService, UnitWork,
};

type Manager = GhostdagManager<
    MemoryGhostdagStore,
    Arc<MemoryRelationsStore>,
    MtReachabilityService<MemoryReachabilityStore>,
    UnitWork,
>;

/// An in-memory blockDAG consensus engine (reachability + GHOSTDAG).
pub struct DagEngine {
    genesis: Hash,
    reach: Arc<RwLock<MemoryReachabilityStore>>,
    gd: Arc<MemoryGhostdagStore>,
    relations: Arc<MemoryRelationsStore>,
    manager: Manager,
    tip: RefCell<Hash>,
}

impl DagEngine {
    /// Create an engine with anticone bound `k`. `genesis` is the first block;
    /// the caller must `add_block(genesis, &[blockhash::ORIGIN])` first.
    pub fn new(k: KType, genesis: Hash) -> Self {
        let reach = Arc::new(RwLock::new(MemoryReachabilityStore::new()));
        inquirer::init(&mut *reach.write().unwrap()).unwrap();
        let gd = Arc::new(MemoryGhostdagStore::new());
        let relations = Arc::new(MemoryRelationsStore::new());
        let service = MtReachabilityService::new(reach.clone());
        let manager = GhostdagManager::new(genesis, k, gd.clone(), relations.clone(), service, UnitWork);
        gd.insert(blockhash::ORIGIN, manager.origin_ghostdag_data()).unwrap();
        Self { genesis, reach, gd, relations, manager, tip: RefCell::new(genesis) }
    }

    /// Add a block referencing `parents`. Runs GHOSTDAG, updates the
    /// reachability index, stores the data, and advances the tip. Returns the
    /// block's GHOSTDAG data.
    pub fn add_block(&self, block: Hash, parents: &[Hash]) -> Arc<GhostdagData> {
        let data = self.manager.ghostdag(parents);
        let sp = data.selected_parent;
        let mergeset: Vec<Hash> = data.unordered_mergeset_without_selected_parent().collect();
        {
            let mut r = self.reach.write().unwrap();
            inquirer::add_block(&mut *r, block, sp, &mut mergeset.iter().cloned()).unwrap();
        }
        inquirer::hint_virtual_selected_parent(&mut *self.reach.write().unwrap(), block).unwrap();

        let data = Arc::new(data);
        self.gd.insert(block, data.clone()).unwrap();
        self.relations.insert(block, BlockHashes::new(parents.to_vec()));

        // Advance the tip (heaviest blue work, ties by larger hash).
        let mut tip = self.tip.borrow_mut();
        let tip_work = self.gd.get_blue_work(*tip).unwrap();
        if data.blue_work > tip_work || (data.blue_work == tip_work && block > *tip) {
            *tip = block;
        }
        data
    }

    /// The current best tip (heaviest blue work).
    pub fn tip(&self) -> Hash {
        *self.tip.borrow()
    }

    pub fn blue_score(&self, block: Hash) -> u64 {
        self.gd.get_blue_score(block).unwrap()
    }

    pub fn selected_parent(&self, block: Hash) -> Hash {
        self.gd.get_selected_parent(block).unwrap()
    }

    /// GHOSTDAG data for a block.
    pub fn data(&self, block: Hash) -> Arc<GhostdagData> {
        self.gd.get_data(block).unwrap()
    }

    /// Is `ancestor` a DAG ancestor of `descendant` (via the reachability index)?
    pub fn is_dag_ancestor_of(&self, ancestor: Hash, descendant: Hash) -> bool {
        inquirer::is_dag_ancestor_of(&*self.reach.read().unwrap(), ancestor, descendant).unwrap()
    }

    /// The full deterministic total order up to the current tip — the GHOSTDAG
    /// linearization. For each block on the selected-parent chain (genesis →
    /// tip), its mergeset (blues then reds, in ascending blue-work order)
    /// precedes it.
    pub fn total_order(&self) -> Vec<Hash> {
        // Selected-parent chain from genesis to tip.
        let mut chain = Vec::new();
        let mut cur = self.tip();
        loop {
            chain.push(cur);
            if cur == self.genesis {
                break;
            }
            cur = self.gd.get_selected_parent(cur).unwrap();
        }
        chain.reverse();

        let mut order = Vec::new();
        for b in chain {
            if b == self.genesis {
                order.push(b);
                continue;
            }
            let d = self.gd.get_data(b).unwrap();
            let mut mergeset: Vec<Hash> = d.unordered_mergeset_without_selected_parent().collect();
            mergeset.sort_by_cached_key(|h| (self.gd.get_blue_work(*h).unwrap(), *h));
            order.extend(mergeset);
            order.push(b);
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u64) -> Hash {
        Hash::from(n)
    }

    /// Build the manual DAG and verify the total order is a valid topological
    /// linearization (each block follows all its parents) covering every block.
    #[test]
    fn total_order_is_topological() {
        let blocks: &[(u64, &[u64])] = &[
            (2, &[1]),
            (3, &[1]),
            (4, &[2, 3]),
            (5, &[4]),
            (6, &[1]),
            (7, &[5, 6]),
            (8, &[1]),
            (9, &[1]),
            (10, &[7, 8, 9]),
            (11, &[1]),
            (12, &[11, 10]),
        ];
        let engine = DagEngine::new(3, h(1));
        engine.add_block(h(1), &[blockhash::ORIGIN]);
        for (id, parents) in blocks {
            let ps: Vec<Hash> = parents.iter().map(|&p| h(p)).collect();
            engine.add_block(h(*id), &ps);
        }

        let order = engine.total_order();
        assert_eq!(order.len(), blocks.len() + 1, "every block ordered once");

        // Topological: each block appears after all its parents.
        let position: std::collections::HashMap<Hash, usize> =
            order.iter().enumerate().map(|(i, h)| (*h, i)).collect();
        for (id, parents) in blocks {
            let bi = position[&h(*id)];
            for &p in *parents {
                assert!(position[&h(p)] < bi, "parent {p} must precede {id}");
            }
        }
        // Genesis is first.
        assert_eq!(order[0], h(1));
    }

    #[test]
    fn tip_is_heaviest() {
        // A long chain outweighs a short side branch; the tip is on the long chain.
        let engine = DagEngine::new(3, h(1));
        engine.add_block(h(1), &[blockhash::ORIGIN]);
        engine.add_block(h(2), &[h(1)]);
        engine.add_block(h(3), &[h(2)]);
        engine.add_block(h(4), &[h(3)]); // long chain tip
        engine.add_block(h(5), &[h(1)]); // short side branch
        assert_eq!(engine.tip(), h(4));
        assert!(engine.is_dag_ancestor_of(h(1), h(4)));
        assert!(!engine.is_dag_ancestor_of(h(5), h(4)));
    }
}
