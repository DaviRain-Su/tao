//! Tests for the ported reachability index: tree-interval validity (incl.
//! reindexing) and DAG reachability queries. Validation helpers are adapted from
//! rusty-kaspa's reachability test utils (ISC).

#![cfg(test)]

use std::collections::{HashSet, VecDeque};

use crate::constants::perf::{DEFAULT_REINDEX_DEPTH, DEFAULT_REINDEX_SLACK};
use crate::interval::Interval;
use crate::store::{MemoryReachabilityStore, ReachabilityStore, ReachabilityStoreReader};
use crate::tree::{add_tree_block, try_advancing_reindex_root};
use crate::{blockhash, inquirer, Hash};

/// Validate the tree-interval invariants over the subtree rooted at `root`:
/// every parent strictly contains each child, consecutive siblings are adjacent,
/// and future-covering sets are interval-ordered.
fn validate_intervals(store: &impl ReachabilityStoreReader, root: Hash) -> Result<(), String> {
    let mut queue = VecDeque::from([root]);
    while let Some(parent) = queue.pop_front() {
        let children = store.get_children(parent).map_err(|e| e.to_string())?;
        queue.extend(children.iter());

        let parent_interval = store.get_interval(parent).map_err(|e| e.to_string())?;
        if parent_interval.is_empty() {
            return Err(format!("empty interval for {parent}"));
        }
        for child in children.iter().cloned() {
            let child_interval = store.get_interval(child).map_err(|e| e.to_string())?;
            if !parent_interval.strictly_contains(child_interval) {
                return Err(format!("child {child} {child_interval} out of parent {parent} {parent_interval}"));
            }
        }
        for siblings in children.windows(2) {
            let a = store.get_interval(siblings[0]).map_err(|e| e.to_string())?;
            let b = store.get_interval(siblings[1]).map_err(|e| e.to_string())?;
            if a.end + 1 != b.start {
                return Err(format!("non-consecutive siblings {a} {b}"));
            }
        }
        let fcs = store.get_future_covering_set(parent).map_err(|e| e.to_string())?;
        for neighbors in fcs.windows(2) {
            let l = store.get_interval(neighbors[0]).map_err(|e| e.to_string())?;
            let r = store.get_interval(neighbors[1]).map_err(|e| e.to_string())?;
            if l.end >= r.start {
                return Err(format!("non-ordered future covering items {l} {r}"));
            }
        }
    }
    Ok(())
}

fn tree_add(store: &mut MemoryReachabilityStore, hash: Hash, parent: Hash) {
    add_tree_block(store, hash, parent, DEFAULT_REINDEX_DEPTH, DEFAULT_REINDEX_SLACK).unwrap();
    try_advancing_reindex_root(store, hash, DEFAULT_REINDEX_DEPTH, DEFAULT_REINDEX_SLACK).unwrap();
}

#[test]
fn tree_intervals_valid_after_reindexing() {
    // Small initial capacity forces reindexing as the tree grows.
    let mut store = MemoryReachabilityStore::new();
    let root: Hash = 1.into();
    inquirer::init_with_params(&mut store, root, Interval::new(1, 15)).unwrap();
    for (hash, parent) in
        [(2, 1), (3, 2), (4, 2), (5, 3), (6, 5), (7, 1), (8, 6), (9, 6), (10, 6), (11, 6)]
    {
        tree_add(&mut store, hash.into(), parent.into());
    }
    validate_intervals(&store, root).unwrap();
}

#[test]
fn tree_intervals_valid_for_deep_chain() {
    // A long chain stresses the reindex (must stay valid throughout).
    let mut store = MemoryReachabilityStore::new();
    let root: Hash = 1.into();
    inquirer::init_with_params(&mut store, root, Interval::maximal()).unwrap();
    for i in 2u64..200 {
        tree_add(&mut store, i.into(), (i - 1).into());
    }
    validate_intervals(&store, root).unwrap();
    // Chain ancestry holds across the whole chain.
    assert!(inquirer::is_chain_ancestor_of(&store, 1.into(), 199.into()).unwrap());
    assert!(inquirer::is_chain_ancestor_of(&store, 100.into(), 150.into()).unwrap());
    assert!(!inquirer::is_chain_ancestor_of(&store, 150.into(), 100.into()).unwrap());
}

// ---- DAG reachability ----

/// A minimal DAG builder over the reachability store (mirrors rusty-kaspa's
/// `DagBuilder`, computing the mergeset via reachability queries instead of a
/// separate relations store).
struct DagTester {
    store: MemoryReachabilityStore,
    parents: crate::BlockHashMap<Vec<Hash>>,
}

impl DagTester {
    fn new() -> Self {
        let mut store = MemoryReachabilityStore::new();
        inquirer::init(&mut store).unwrap();
        Self { store, parents: crate::BlockHashMap::new() }
    }

    fn add(&mut self, id: u64, parent_ids: &[Hash]) {
        let block: Hash = id.into();
        let parents = parent_ids.to_vec();
        // Selected parent = highest tree height (longest chain), as in Kaspa's tests.
        let sp = *parents.iter().max_by_key(|p| self.store.get_height(**p).unwrap()).unwrap();
        let mergeset = self.mergeset(sp, &parents);
        inquirer::add_block(&mut self.store, block, sp, &mut mergeset.iter().cloned()).unwrap();
        inquirer::hint_virtual_selected_parent(&mut self.store, block).unwrap();
        self.parents.insert(block, parents);
    }

    /// Mergeset = ancestors reachable from `parents` that are not in the selected
    /// parent's past (the anticone of the selected chain within the new block's past).
    fn mergeset(&self, sp: Hash, parents: &[Hash]) -> Vec<Hash> {
        let mut queue: VecDeque<Hash> = parents.iter().cloned().filter(|p| *p != sp).collect();
        let mut visited: HashSet<Hash> = HashSet::new();
        let mut out = Vec::new();
        while let Some(c) = queue.pop_front() {
            if !visited.insert(c) || c == sp {
                continue;
            }
            if inquirer::is_dag_ancestor_of(&self.store, c, sp).unwrap() {
                continue; // already in the selected parent's past
            }
            out.push(c);
            for gp in self.parents.get(&c).into_iter().flatten() {
                queue.push_back(*gp);
            }
        }
        out
    }

    fn in_past_of(&self, block: u64, other: u64) -> bool {
        if block == other {
            return false;
        }
        let res = inquirer::is_dag_ancestor_of(&self.store, block.into(), other.into()).unwrap();
        if res {
            // future relation is asymmetric
            assert!(!inquirer::is_dag_ancestor_of(&self.store, other.into(), block.into()).unwrap());
        }
        res
    }

    fn are_anticone(&self, a: u64, b: u64) -> bool {
        !inquirer::is_dag_ancestor_of(&self.store, a.into(), b.into()).unwrap()
            && !inquirer::is_dag_ancestor_of(&self.store, b.into(), a.into()).unwrap()
    }
}

#[test]
fn dag_reachability_queries() {
    // The manual DAG from rusty-kaspa's reachability test.
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
    let expected_past = [(2, 4), (2, 5), (2, 7), (5, 10), (6, 10), (10, 12), (11, 12)];
    let expected_anticone = [(2, 3), (2, 6), (3, 6), (5, 6), (3, 8), (11, 2), (11, 4), (11, 6), (11, 9)];

    let mut dag = DagTester::new();
    dag.add(1, &[blockhash::ORIGIN]); // genesis
    for (id, parents) in blocks {
        let ps: Vec<Hash> = parents.iter().map(|&p| Hash::from(p)).collect();
        dag.add(*id, &ps);
    }

    // Genesis is in the past of every other block.
    for (id, _) in blocks {
        assert!(dag.in_past_of(1, *id), "genesis must be in past of {id}");
    }
    for (x, y) in expected_past {
        assert!(dag.in_past_of(x, y), "{x} expected in past of {y}");
    }
    for (x, y) in expected_anticone {
        assert!(dag.are_anticone(x, y), "{x} and {y} expected to be anticone");
    }
    validate_intervals(&dag.store, blockhash::ORIGIN).unwrap();
}
