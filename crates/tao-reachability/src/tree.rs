// Ported from rusty-kaspa, ISC, consensus/src/processes/reachability/tree.rs.
// Adapted: crate-local module paths and `Hash` type.

//! Tree-related functions internal to the module.

use crate::extensions::ReachabilityStoreIntervalExtensions;
use crate::inquirer::{get_next_chain_ancestor_unchecked, is_chain_ancestor_of};
use crate::reindex::ReindexOperationContext;
use crate::store::ReachabilityStore;
use crate::{Hash, ReachabilityError, Result};

/// Adds `new_block` as a child of `parent` in the tree. If there's no remaining
/// interval to allocate, a reindexing is triggered (using the reindex root).
pub(crate) fn add_tree_block(
    store: &mut (impl ReachabilityStore + ?Sized),
    new_block: Hash,
    parent: Hash,
    reindex_depth: u64,
    reindex_slack: u64,
) -> Result<()> {
    let remaining = store.interval_remaining_after(parent)?;
    store.append_child(parent, new_block)?;
    let parent_height = store.get_height(parent)?;
    if remaining.is_empty() {
        // Init with the empty interval that comes exactly at the end of capacity.
        store.insert(new_block, parent, remaining, parent_height + 1)?;
        let reindex_root = store.get_reindex_root()?;
        let mut ctx = ReindexOperationContext::new(store, reindex_depth, reindex_slack);
        ctx.reindex_intervals(new_block, reindex_root)?;
    } else {
        let allocated = remaining.split_half().0;
        store.insert(new_block, parent, allocated, parent_height + 1)?;
    };
    Ok(())
}

/// Finds the most recent tree ancestor common to both `block` and `reindex_root`.
pub(crate) fn find_common_tree_ancestor(
    store: &(impl ReachabilityStore + ?Sized),
    block: Hash,
    reindex_root: Hash,
) -> Result<Hash> {
    let mut current = block;
    loop {
        if is_chain_ancestor_of(store, current, reindex_root)? {
            return Ok(current);
        }
        current = store.get_parent(current)?;
    }
}

/// Finds a possible new reindex root, based on `current` and the selected tip `hint`.
pub(crate) fn find_next_reindex_root(
    store: &(impl ReachabilityStore + ?Sized),
    current: Hash,
    hint: Hash,
    reindex_depth: u64,
    reindex_slack: u64,
) -> Result<(Hash, Hash)> {
    let mut ancestor = current;
    let mut next = current;

    let hint_height = store.get_height(hint)?;

    if !is_chain_ancestor_of(store, current, hint)? {
        let current_height = store.get_height(current)?;
        // Switch chains only after a sufficient `reindex_slack` diff (to resist
        // alternating-reorg attacks).
        if hint_height < current_height || hint_height - current_height < reindex_slack {
            return Ok((current, current));
        }
        let common = find_common_tree_ancestor(store, hint, current)?;
        ancestor = common;
        next = common;
    }

    loop {
        let child = get_next_chain_ancestor_unchecked(store, hint, next)?;
        let child_height = store.get_height(child)?;

        if hint_height < child_height {
            return Err(ReachabilityError::DataInconsistency);
        }
        if hint_height - child_height < reindex_depth {
            break;
        }
        next = child;
    }

    Ok((ancestor, next))
}

/// Attempts to advance/move the current reindex root toward the `virtual selected
/// parent` hint, keeping the root on the consensus-agreed selected chain.
pub(crate) fn try_advancing_reindex_root(
    store: &mut (impl ReachabilityStore + ?Sized),
    hint: Hash,
    reindex_depth: u64,
    reindex_slack: u64,
) -> Result<()> {
    let current = store.get_reindex_root()?;
    let (mut ancestor, next) = find_next_reindex_root(store, current, hint, reindex_depth, reindex_slack)?;

    if current == next {
        return Ok(());
    }

    while ancestor != next {
        let child = get_next_chain_ancestor_unchecked(store, next, ancestor)?;
        let mut ctx = ReindexOperationContext::new(store, reindex_depth, reindex_slack);
        ctx.concentrate_interval(ancestor, child, child == next)?;
        ancestor = child;
    }

    store.set_reindex_root(next)?;
    Ok(())
}
