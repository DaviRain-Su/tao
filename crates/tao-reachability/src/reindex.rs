// Ported from rusty-kaspa, ISC, consensus/src/processes/reachability/reindex.rs.
// Adapted: crate-local module paths, `Hash`/`BlockHashMap` types, `is_none()`
// method instead of `BlockHashExtensions`, and the kaspa-coupled test module
// dropped (covered by this crate's own tests).

use std::collections::VecDeque;

use crate::extensions::ReachabilityStoreIntervalExtensions;
use crate::inquirer::{self, get_next_chain_ancestor_unchecked};
use crate::interval::Interval;
use crate::store::ReachabilityStore;
use crate::{BlockHashMap, Hash, ReachabilityError, Result};

/// Temporary context for caching subtree information during a *single* reindex.
pub(crate) struct ReindexOperationContext<'a, T: ReachabilityStore + ?Sized> {
    store: &'a mut T,
    subtree_sizes: BlockHashMap<u64>,
    _depth: u64,
    slack: u64,
}

impl<'a, T: ReachabilityStore + ?Sized> ReindexOperationContext<'a, T> {
    pub(crate) fn new(store: &'a mut T, depth: u64, slack: u64) -> Self {
        Self {
            store,
            subtree_sizes: BlockHashMap::new(),
            _depth: depth,
            slack,
        }
    }

    /// Traverses the reachability subtree defined by the new child block and
    /// reallocates interval space so another reindex is unlikely soon, by
    /// walking down until a block whose interval size exceeds its subtree size.
    pub(crate) fn reindex_intervals(&mut self, new_child: Hash, reindex_root: Hash) -> Result<()> {
        let mut current = new_child;

        loop {
            let current_interval = self.store.get_interval(current)?;
            self.count_subtrees(current)?;

            if current_interval.size() >= self.subtree_sizes[&current] {
                break;
            }

            let parent = self.store.get_parent(current)?;

            if parent.is_none() {
                return Err(ReachabilityError::DataOverflow(
                    "missing tree parent during reindexing. Theoretically, this should only ever \
                     happen if there are more than 2^64 blocks in the DAG."
                        .to_string(),
                ));
            }

            if current == reindex_root {
                return Err(ReachabilityError::DataOverflow(format!(
                    "unexpected behavior: reindex root {reindex_root} is out of capacity during reindexing. \
                     Theoretically, this should only ever happen if there are more than ~2^52 blocks in the DAG."
                )));
            }

            if inquirer::is_strict_chain_ancestor_of(self.store, parent, reindex_root)? {
                // parent is guaranteed to have sufficient interval space; avoid
                // reindexing the entire (huge) subtree above it by using slacks
                // along the chain forward to the reindex root.
                return self.reindex_intervals_earlier_than_root(
                    current,
                    reindex_root,
                    parent,
                    self.subtree_sizes[&current],
                );
            }

            current = parent
        }

        self.propagate_interval(current)
    }

    /// Counts the size of each subtree under `block` into `self.subtree_sizes`.
    /// Implemented BFS (not recursively) to tolerate linearly-deep trees.
    fn count_subtrees(&mut self, block: Hash) -> Result<()> {
        if self.subtree_sizes.contains_key(&block) {
            return Ok(());
        }

        let mut queue = VecDeque::<Hash>::from([block]);
        let mut counts = BlockHashMap::<u64>::new();

        while let Some(mut current) = queue.pop_front() {
            let children = self.store.get_children(current)?;
            if children.is_empty() {
                self.subtree_sizes.insert(current, 1);
            } else if !self.subtree_sizes.contains_key(&current) {
                queue.extend(children.iter());
                continue;
            }

            // Reached a leaf or a pre-calculated subtree; push information up.
            while current != block {
                current = self.store.get_parent(current)?;

                let count = counts.entry(current).or_insert(0);
                let children = self.store.get_children(current)?;

                *count += 1;
                if *count < children.len() as u64 {
                    break;
                }

                let subtree_sum: u64 = children.iter().map(|c| self.subtree_sizes[c]).sum();
                self.subtree_sizes.insert(current, subtree_sum + 1);
            }
        }

        Ok(())
    }

    /// Propagates a new interval using BFS, allocating subtree intervals by
    /// subtree size via `Interval::split_exponential`.
    fn propagate_interval(&mut self, block: Hash) -> Result<()> {
        self.count_subtrees(block)?;

        let mut queue = VecDeque::<Hash>::from([block]);
        while let Some(current) = queue.pop_front() {
            let children = self.store.get_children(current)?;
            if !children.is_empty() {
                let sizes: Vec<u64> = children.iter().map(|c| self.subtree_sizes[c]).collect();
                let interval = self.store.interval_children_capacity(current)?;
                let intervals = interval.split_exponential(&sizes);
                for (c, ci) in children.iter().copied().zip(intervals) {
                    self.store.set_interval(c, ci)?;
                }
                queue.extend(children.iter());
            }
        }
        Ok(())
    }

    /// Reindex for the case where the new child is not in the reindex root's
    /// subtree, allocating `required_allocation` to `allocation_block`.
    fn reindex_intervals_earlier_than_root(
        &mut self,
        allocation_block: Hash,
        reindex_root: Hash,
        common_ancestor: Hash,
        required_allocation: u64,
    ) -> Result<()> {
        let chosen_child =
            get_next_chain_ancestor_unchecked(self.store, reindex_root, common_ancestor)?;
        let block_interval = self.store.get_interval(allocation_block)?;
        let chosen_interval = self.store.get_interval(chosen_child)?;

        if block_interval.start < chosen_interval.start {
            self.reclaim_interval_before(
                allocation_block,
                common_ancestor,
                chosen_child,
                reindex_root,
                required_allocation,
            )
        } else {
            self.reclaim_interval_after(
                allocation_block,
                common_ancestor,
                chosen_child,
                reindex_root,
                required_allocation,
            )
        }
    }

    fn reclaim_interval_before(
        &mut self,
        allocation_block: Hash,
        common_ancestor: Hash,
        chosen_child: Hash,
        reindex_root: Hash,
        required_allocation: u64,
    ) -> Result<()> {
        let mut slack_sum = 0u64;
        let mut path_len = 0u64;
        let mut path_slack_alloc = 0u64;

        let mut current = chosen_child;
        loop {
            if current == reindex_root {
                let offset = required_allocation + self.slack * path_len - slack_sum;
                self.apply_interval_op_and_propagate(current, offset, Interval::increase_start)?;
                self.offset_siblings_before(allocation_block, current, offset)?;
                path_slack_alloc = self.slack;
                break;
            }

            let slack_before_current = self.store.interval_remaining_before(current)?.size();
            slack_sum += slack_before_current;

            if slack_sum >= required_allocation {
                let offset = slack_before_current - (slack_sum - required_allocation);
                self.apply_interval_op(current, offset, Interval::increase_start)?;
                self.offset_siblings_before(allocation_block, current, offset)?;
                break;
            }

            current = get_next_chain_ancestor_unchecked(self.store, reindex_root, current)?;
            path_len += 1;
        }

        loop {
            current = self.store.get_parent(current)?;
            if current == common_ancestor {
                break;
            }
            let slack_before_current = self.store.interval_remaining_before(current)?.size();
            let offset = slack_before_current - path_slack_alloc;
            self.apply_interval_op(current, offset, Interval::increase_start)?;
            self.offset_siblings_before(allocation_block, current, offset)?;
        }

        Ok(())
    }

    fn reclaim_interval_after(
        &mut self,
        allocation_block: Hash,
        common_ancestor: Hash,
        chosen_child: Hash,
        reindex_root: Hash,
        required_allocation: u64,
    ) -> Result<()> {
        let mut slack_sum = 0u64;
        let mut path_len = 0u64;
        let mut path_slack_alloc = 0u64;

        let mut current = chosen_child;
        loop {
            if current == reindex_root {
                let offset = required_allocation + self.slack * path_len - slack_sum;
                self.apply_interval_op_and_propagate(current, offset, Interval::decrease_end)?;
                self.offset_siblings_after(allocation_block, current, offset)?;
                path_slack_alloc = self.slack;
                break;
            }

            let slack_after_current = self.store.interval_remaining_after(current)?.size();
            slack_sum += slack_after_current;

            if slack_sum >= required_allocation {
                let offset = slack_after_current - (slack_sum - required_allocation);
                self.apply_interval_op(current, offset, Interval::decrease_end)?;
                self.offset_siblings_after(allocation_block, current, offset)?;
                break;
            }

            current = get_next_chain_ancestor_unchecked(self.store, reindex_root, current)?;
            path_len += 1;
        }

        loop {
            current = self.store.get_parent(current)?;
            if current == common_ancestor {
                break;
            }
            let slack_after_current = self.store.interval_remaining_after(current)?.size();
            let offset = slack_after_current - path_slack_alloc;
            self.apply_interval_op(current, offset, Interval::decrease_end)?;
            self.offset_siblings_after(allocation_block, current, offset)?;
        }

        Ok(())
    }

    fn offset_siblings_before(
        &mut self,
        allocation_block: Hash,
        current: Hash,
        offset: u64,
    ) -> Result<()> {
        let parent = self.store.get_parent(current)?;
        let children = self.store.get_children(parent)?;

        let (siblings_before, _) = split_children(&children, current)?;
        for sibling in siblings_before.iter().cloned().rev() {
            if sibling == allocation_block {
                self.apply_interval_op_and_propagate(
                    allocation_block,
                    offset,
                    Interval::increase_end,
                )?;
                break;
            }
            self.apply_interval_op_and_propagate(sibling, offset, Interval::increase)?;
        }

        Ok(())
    }

    fn offset_siblings_after(
        &mut self,
        allocation_block: Hash,
        current: Hash,
        offset: u64,
    ) -> Result<()> {
        let parent = self.store.get_parent(current)?;
        let children = self.store.get_children(parent)?;

        let (_, siblings_after) = split_children(&children, current)?;
        for sibling in siblings_after.iter().cloned() {
            if sibling == allocation_block {
                self.apply_interval_op_and_propagate(
                    allocation_block,
                    offset,
                    Interval::decrease_start,
                )?;
                break;
            }
            self.apply_interval_op_and_propagate(sibling, offset, Interval::decrease)?;
        }

        Ok(())
    }

    fn apply_interval_op(
        &mut self,
        block: Hash,
        offset: u64,
        op: fn(&Interval, u64) -> Interval,
    ) -> Result<()> {
        self.store
            .set_interval(block, op(&self.store.get_interval(block)?, offset))?;
        Ok(())
    }

    fn apply_interval_op_and_propagate(
        &mut self,
        block: Hash,
        offset: u64,
        op: fn(&Interval, u64) -> Interval,
    ) -> Result<()> {
        self.store
            .set_interval(block, op(&self.store.get_interval(block)?, offset))?;
        self.propagate_interval(block)?;
        Ok(())
    }

    /// Handles reindex operations triggered by moving the reindex root.
    pub(crate) fn concentrate_interval(
        &mut self,
        parent: Hash,
        child: Hash,
        is_final_reindex_root: bool,
    ) -> Result<()> {
        let children = self.store.get_children(parent)?;
        let (siblings_before, siblings_after) = split_children(&children, child)?;

        let siblings_before_subtrees_sum: u64 =
            self.tighten_intervals_before(parent, siblings_before)?;
        let siblings_after_subtrees_sum: u64 =
            self.tighten_intervals_after(parent, siblings_after)?;

        self.expand_interval_to_chosen(
            parent,
            child,
            siblings_before_subtrees_sum,
            siblings_after_subtrees_sum,
            is_final_reindex_root,
        )?;

        Ok(())
    }

    fn tighten_intervals_before(&mut self, parent: Hash, children_before: &[Hash]) -> Result<u64> {
        let sizes = children_before
            .iter()
            .cloned()
            .map(|block| {
                self.count_subtrees(block)?;
                Ok(self.subtree_sizes[&block])
            })
            .collect::<Result<Vec<u64>>>()?;
        let sum = sizes.iter().sum();

        let interval = self.store.get_interval(parent)?;
        let interval_before = Interval::new(
            interval.start + self.slack,
            interval.start + self.slack + sum - 1,
        );

        for (c, ci) in children_before
            .iter()
            .cloned()
            .zip(interval_before.split_exact(sizes.as_slice()))
        {
            self.store.set_interval(c, ci)?;
            self.propagate_interval(c)?;
        }

        Ok(sum)
    }

    fn tighten_intervals_after(&mut self, parent: Hash, children_after: &[Hash]) -> Result<u64> {
        let sizes = children_after
            .iter()
            .cloned()
            .map(|block| {
                self.count_subtrees(block)?;
                Ok(self.subtree_sizes[&block])
            })
            .collect::<Result<Vec<u64>>>()?;
        let sum = sizes.iter().sum();

        let interval = self.store.get_interval(parent)?;
        let interval_after = Interval::new(
            interval.end - self.slack - sum,
            interval.end - self.slack - 1,
        );

        for (c, ci) in children_after
            .iter()
            .cloned()
            .zip(interval_after.split_exact(sizes.as_slice()))
        {
            self.store.set_interval(c, ci)?;
            self.propagate_interval(c)?;
        }

        Ok(sum)
    }

    fn expand_interval_to_chosen(
        &mut self,
        parent: Hash,
        child: Hash,
        siblings_before_subtrees_sum: u64,
        siblings_after_subtrees_sum: u64,
        is_final_reindex_root: bool,
    ) -> Result<()> {
        let interval = self.store.get_interval(parent)?;
        let allocation = Interval::new(
            interval.start + siblings_before_subtrees_sum + self.slack,
            interval.end - siblings_after_subtrees_sum - self.slack - 1,
        );
        let current = self.store.get_interval(child)?;

        if is_final_reindex_root && !allocation.contains(current) {
            let narrowed =
                Interval::new(allocation.start + self.slack, allocation.end - self.slack);
            self.store.set_interval(child, narrowed)?;
            self.propagate_interval(child)?;
        }

        self.store.set_interval(child, allocation)?;
        Ok(())
    }
}

/// Splits `children` into the blocks before `pivot` and the blocks after.
fn split_children(children: &std::sync::Arc<Vec<Hash>>, pivot: Hash) -> Result<(&[Hash], &[Hash])> {
    if let Some(index) = children.iter().cloned().position(|c| c == pivot) {
        Ok((&children[..index], &children[index + 1..]))
    } else {
        Err(ReachabilityError::DataInconsistency)
    }
}
