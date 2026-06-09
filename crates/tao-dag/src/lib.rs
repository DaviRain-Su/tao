//! `tao-dag` — a GHOSTDAG blockDAG ordering prototype.
//!
//! This is the core of Kaspa's consensus (the PHANTOM/GHOSTDAG protocol): blocks
//! reference *multiple* parents, so concurrently-mined blocks coexist instead of
//! being orphaned, and the chain can run far above the network delay. GHOSTDAG
//! then deterministically classifies every block as **blue** (well-connected,
//! rewarded) or **red** (mined in ignorance of too much of the DAG — the
//! signature of withholding) and produces a single **total order** over all
//! blocks that every node reproduces identically.
//!
//! ## Scope (honest)
//!
//! This implements the GHOSTDAG ordering algorithm itself, with explicit `past`
//! sets — clear and correct for modest DAGs, the right way to *prove the
//! algorithm*. Productionizing it (the rest of plan M8) needs the hard systems
//! work this prototype omits: an efficient **reachability index** (so "is X in
//! the anticone of Y?" is sub-millisecond instead of a set scan), **pruning** of
//! a growing DAG, and — to keep Solana-SVM compatibility — feeding GHOSTDAG's
//! linear order into the SVM (the "Case B" linearization problem). Block ids are
//! `u64` here for readability; production uses 32-byte header hashes.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

/// A block identifier (production: a 32-byte header hash).
pub type BlockId = u64;

/// Errors when adding a block.
#[derive(Debug, PartialEq, Eq)]
pub enum DagError {
    Duplicate,
    UnknownParent,
    NoParents,
}

struct Node {
    parents: Vec<BlockId>,
    /// Ancestors (exclusive of self).
    past: HashSet<BlockId>,
    /// All blue blocks in `past ∪ {self}` (self is always blue in its own view).
    blue_set: HashSet<BlockId>,
    selected_parent: Option<BlockId>,
    /// Blocks this block merges (in `past` but not in the selected parent's
    /// past), split into blue/red, each in deterministic topological order.
    mergeset_blues: Vec<BlockId>,
    mergeset_reds: Vec<BlockId>,
}

/// A GHOSTDAG-ordered blockDAG.
pub struct Dag {
    k: usize,
    genesis: BlockId,
    nodes: HashMap<BlockId, Node>,
}

impl Dag {
    /// Create a DAG with anticone bound `k` and the given genesis block.
    pub fn new(k: usize, genesis: BlockId) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            genesis,
            Node {
                parents: vec![],
                past: HashSet::new(),
                blue_set: HashSet::from([genesis]),
                selected_parent: None,
                mergeset_blues: vec![],
                mergeset_reds: vec![],
            },
        );
        Self { k, genesis, nodes }
    }

    pub fn contains(&self, id: BlockId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Blue score = number of blue blocks in `past(id) ∪ {id}`.
    pub fn blue_score(&self, id: BlockId) -> u64 {
        self.nodes[&id].blue_set.len() as u64
    }

    pub fn selected_parent(&self, id: BlockId) -> Option<BlockId> {
        self.nodes[&id].selected_parent
    }

    /// The parents (referenced tips) of a block.
    pub fn parents(&self, id: BlockId) -> &[BlockId] {
        &self.nodes[&id].parents
    }

    /// Is `block` blue in the view of the current tip?
    pub fn is_blue_at_tip(&self, block: BlockId) -> bool {
        self.nodes[&self.tip()].blue_set.contains(&block)
    }

    fn is_anticone(&self, x: BlockId, c: BlockId) -> bool {
        x != c && !self.nodes[&c].past.contains(&x) && !self.nodes[&x].past.contains(&c)
    }

    /// Deterministic topological key: ancestors (smaller past) first, ties by id.
    fn topo_key(&self, b: BlockId) -> (usize, BlockId) {
        (self.nodes[&b].past.len(), b)
    }

    /// Add a block referencing `parents` (which must already exist).
    pub fn add_block(&mut self, id: BlockId, parents: Vec<BlockId>) -> Result<(), DagError> {
        if self.nodes.contains_key(&id) {
            return Err(DagError::Duplicate);
        }
        if parents.is_empty() {
            return Err(DagError::NoParents);
        }
        if parents.iter().any(|p| !self.nodes.contains_key(p)) {
            return Err(DagError::UnknownParent);
        }

        // past(id) = ∪ (past(p) ∪ {p})
        let mut past = HashSet::new();
        for p in &parents {
            past.insert(*p);
            past.extend(self.nodes[p].past.iter().copied());
        }

        // Selected parent = highest blue score, ties broken by smaller id.
        let sp = *parents
            .iter()
            .max_by_key(|p| (self.blue_score(**p), Reverse(**p)))
            .unwrap();

        // Mergeset = past(id) \ (past(sp) ∪ {sp}), in topological order.
        let sp_past = &self.nodes[&sp].past;
        let mut mergeset: Vec<BlockId> = past
            .iter()
            .copied()
            .filter(|b| *b != sp && !sp_past.contains(b))
            .collect();
        mergeset.sort_by_key(|b| self.topo_key(*b));

        // Greedy GHOSTDAG k-cluster classification, seeded by the selected
        // parent's blue set.
        let mut accepted: Vec<BlockId> = self.nodes[&sp].blue_set.iter().copied().collect();
        let mut mergeset_blues = Vec::new();
        let mut mergeset_reds = Vec::new();
        for &c in &mergeset {
            let anti: Vec<BlockId> = accepted
                .iter()
                .copied()
                .filter(|&x| self.is_anticone(x, c))
                .collect();
            // Condition 1: c's blue anticone is within k.
            if anti.len() > self.k {
                mergeset_reds.push(c);
                continue;
            }
            // Condition 2: adding c keeps every affected blue within k too.
            let mut ok = true;
            for &x in &anti {
                let x_anti = accepted.iter().filter(|&&y| self.is_anticone(y, x)).count();
                if x_anti + 1 > self.k {
                    ok = false;
                    break;
                }
            }
            if !ok {
                mergeset_reds.push(c);
                continue;
            }
            accepted.push(c);
            mergeset_blues.push(c);
        }

        let mut blue_set: HashSet<BlockId> = self.nodes[&sp].blue_set.clone();
        blue_set.extend(mergeset_blues.iter().copied());
        blue_set.insert(id);

        self.nodes.insert(
            id,
            Node {
                parents,
                past,
                blue_set,
                selected_parent: Some(sp),
                mergeset_blues,
                mergeset_reds,
            },
        );
        Ok(())
    }

    /// The best tip = highest blue score (ties by smaller id).
    pub fn tip(&self) -> BlockId {
        *self
            .nodes
            .keys()
            .max_by_key(|id| (self.blue_score(**id), Reverse(**id)))
            .unwrap()
    }

    /// The full deterministic total order of all blocks up to the best tip.
    ///
    /// `ORDER(B) = ORDER(selectedParent(B)) ++ mergeset_blues ++ mergeset_reds ++ [B]`.
    pub fn order(&self) -> Vec<BlockId> {
        // Selected-parent chain from genesis to tip.
        let mut chain = Vec::new();
        let mut cur = Some(self.tip());
        while let Some(b) = cur {
            chain.push(b);
            cur = self.nodes[&b].selected_parent;
        }
        chain.reverse();

        let mut out = Vec::new();
        for b in chain {
            if b == self.genesis {
                out.push(b);
                continue;
            }
            let node = &self.nodes[&b];
            let mut blues = node.mergeset_blues.clone();
            let mut reds = node.mergeset_reds.clone();
            blues.sort_by_key(|x| self.topo_key(*x));
            reds.sort_by_key(|x| self.topo_key(*x));
            out.extend(blues);
            out.extend(reds);
            out.push(b);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_chain_is_all_blue_and_ordered() {
        let mut dag = Dag::new(3, 0);
        dag.add_block(1, vec![0]).unwrap();
        dag.add_block(2, vec![1]).unwrap();
        dag.add_block(3, vec![2]).unwrap();
        assert_eq!(dag.order(), vec![0, 1, 2, 3]);
        assert_eq!(dag.blue_score(3), 4);
        assert!(dag.is_blue_at_tip(2));
    }

    #[test]
    fn diamond_merges_parallel_block_as_blue() {
        // 0 → {1,2} → 3 ; with k≥1 the parallel block is blue.
        let mut dag = Dag::new(1, 0);
        dag.add_block(1, vec![0]).unwrap();
        dag.add_block(2, vec![0]).unwrap();
        dag.add_block(3, vec![1, 2]).unwrap();
        assert_eq!(dag.order(), vec![0, 1, 2, 3]);
        assert_eq!(dag.blue_score(3), 4); // 0,1,2,3 all blue
        assert!(dag.is_blue_at_tip(2));
    }

    #[test]
    fn k_zero_degenerates_to_chain() {
        // With k=0 GHOSTDAG is the longest-chain rule: the parallel block is red.
        let mut dag = Dag::new(0, 0);
        dag.add_block(1, vec![0]).unwrap();
        dag.add_block(2, vec![0]).unwrap();
        dag.add_block(3, vec![1, 2]).unwrap();
        assert_eq!(dag.blue_score(3), 3); // 0,1,3 blue; 2 red
        assert!(!dag.is_blue_at_tip(2));
        // Every block still appears exactly once, in a deterministic order.
        let mut sorted = dag.order();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
    }

    #[test]
    fn block_ignoring_the_dag_is_marked_red() {
        // Honest chain 0→1→2→3; block 4 references only genesis (ignores 1,2,3);
        // tip 5 merges both. With k=1, block 4's large anticone makes it red.
        let mut dag = Dag::new(1, 0);
        dag.add_block(1, vec![0]).unwrap();
        dag.add_block(2, vec![1]).unwrap();
        dag.add_block(3, vec![2]).unwrap();
        dag.add_block(4, vec![0]).unwrap(); // mined in ignorance of 1,2,3
        dag.add_block(5, vec![3, 4]).unwrap();
        assert_eq!(dag.selected_parent(5), Some(3)); // heavier branch
        assert!(!dag.is_blue_at_tip(4), "block ignoring the DAG must be red");
        assert!(dag.is_blue_at_tip(3));
        let mut sorted = dag.order();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5]); // all blocks ordered once
    }

    #[test]
    fn order_is_deterministic() {
        let build = || {
            let mut dag = Dag::new(2, 0);
            dag.add_block(1, vec![0]).unwrap();
            dag.add_block(2, vec![0]).unwrap();
            dag.add_block(3, vec![0]).unwrap();
            dag.add_block(4, vec![1, 2, 3]).unwrap();
            dag.order()
        };
        assert_eq!(build(), build());
    }
}
