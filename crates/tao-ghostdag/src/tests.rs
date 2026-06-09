//! Tests: run the ported GHOSTDAG over the ported reachability index, building
//! real DAGs and checking blue/red classification + blue score.

#![cfg(test)]

use std::sync::{Arc, RwLock};

use tao_reachability::{blockhash, inquirer, BlockHashes, Hash, MemoryReachabilityStore};

use crate::{
    GhostdagManager, GhostdagStore, GhostdagStoreReader, KType, MemoryGhostdagStore,
    MemoryRelationsStore, MtReachabilityService, UnitWork,
};

type Manager = GhostdagManager<
    MemoryGhostdagStore,
    Arc<MemoryRelationsStore>,
    MtReachabilityService<MemoryReachabilityStore>,
    UnitWork,
>;

/// A DAG driver wiring GHOSTDAG + reachability together (as the consensus layer does).
struct Gd {
    reach: Arc<RwLock<MemoryReachabilityStore>>,
    gd_store: Arc<MemoryGhostdagStore>,
    relations: Arc<MemoryRelationsStore>,
    manager: Manager,
}

impl Gd {
    fn new(k: KType, genesis: Hash) -> Self {
        let reach = Arc::new(RwLock::new(MemoryReachabilityStore::new()));
        inquirer::init(&mut *reach.write().unwrap()).unwrap(); // creates ORIGIN
        let gd_store = Arc::new(MemoryGhostdagStore::new());
        let relations = Arc::new(MemoryRelationsStore::new());
        let service = MtReachabilityService::new(reach.clone());
        let manager = GhostdagManager::new(genesis, k, gd_store.clone(), relations.clone(), service, UnitWork);
        gd_store.insert(blockhash::ORIGIN, manager.origin_ghostdag_data()).unwrap();
        Self { reach, gd_store, relations, manager }
    }

    fn add(&self, id: u64, parents: &[Hash]) {
        let block: Hash = id.into();
        let data = self.manager.ghostdag(parents);

        // Update reachability with the same selected parent + mergeset.
        let sp = data.selected_parent;
        let mergeset: Vec<Hash> = data.unordered_mergeset_without_selected_parent().collect();
        {
            let mut r = self.reach.write().unwrap();
            inquirer::add_block(&mut *r, block, sp, &mut mergeset.iter().cloned()).unwrap();
        }
        inquirer::hint_virtual_selected_parent(&mut *self.reach.write().unwrap(), block).unwrap();

        self.gd_store.insert(block, Arc::new(data)).unwrap();
        self.relations.insert(block, BlockHashes::new(parents.to_vec()));
    }

    fn data(&self, id: u64) -> Arc<crate::GhostdagData> {
        self.gd_store.get_data(id.into()).unwrap()
    }

    fn is_blue_in(&self, container: u64, block: u64) -> bool {
        self.data(container).mergeset_blues.contains(&Hash::from(block))
    }

    fn is_red_in(&self, container: u64, block: u64) -> bool {
        self.data(container).mergeset_reds.contains(&Hash::from(block))
    }
}

fn h(n: u64) -> Hash {
    Hash::from(n)
}

#[test]
fn linear_chain_blue_scores_increase() {
    let dag = Gd::new(3, h(1));
    dag.add(1, &[blockhash::ORIGIN]);
    dag.add(2, &[h(1)]);
    dag.add(3, &[h(2)]);
    dag.add(4, &[h(3)]);
    assert_eq!(dag.data(1).blue_score, 0);
    assert_eq!(dag.data(2).blue_score, 1);
    assert_eq!(dag.data(3).blue_score, 2);
    assert_eq!(dag.data(4).blue_score, 3);
}

#[test]
fn diamond_parallel_block_is_blue_with_large_k() {
    // 1; 2->1; 3->1; 4->{2,3}. With k≥2 the second parent is blue.
    let dag = Gd::new(18, h(1));
    dag.add(1, &[blockhash::ORIGIN]);
    dag.add(2, &[h(1)]);
    dag.add(3, &[h(1)]);
    dag.add(4, &[h(2), h(3)]);
    let d4 = dag.data(4);
    assert_eq!(d4.mergeset_reds.len(), 0, "no reds with large k");
    assert_eq!(d4.mergeset_blues.len(), 2, "selected parent + merged blue");
    assert_eq!(d4.blue_score, 3, "1,2,3 all blue in 4's past");
}

#[test]
fn diamond_degenerates_to_chain_with_k_zero() {
    // With k=0 GHOSTDAG is the longest chain: the parallel block is red.
    let dag = Gd::new(0, h(1));
    dag.add(1, &[blockhash::ORIGIN]);
    dag.add(2, &[h(1)]);
    dag.add(3, &[h(1)]);
    dag.add(4, &[h(2), h(3)]);
    let d4 = dag.data(4);
    assert_eq!(d4.mergeset_reds.len(), 1, "parallel block is red with k=0");
    assert_eq!(d4.blue_score, 2, "only the selected chain is blue");
}

#[test]
fn block_ignoring_the_chain_is_red() {
    // Honest chain 1→2→3→4; block 5 references only genesis (ignores 2,3,4);
    // block 6 merges {4,5}. With k=1 block 5's large anticone makes it red.
    let dag = Gd::new(1, h(1));
    dag.add(1, &[blockhash::ORIGIN]);
    dag.add(2, &[h(1)]);
    dag.add(3, &[h(2)]);
    dag.add(4, &[h(3)]);
    dag.add(5, &[h(1)]); // mined ignoring 2,3,4
    dag.add(6, &[h(4), h(5)]);

    assert_eq!(dag.data(6).selected_parent, h(4), "heavier chain selected");
    assert!(dag.is_red_in(6, 5), "block ignoring the DAG must be red");
    assert!(!dag.is_blue_in(6, 5));
}
