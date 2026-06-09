// Ported from rusty-kaspa, ISC, consensus/src/processes/reachability/inquirer.rs.
// Adapted: crate-local module paths, `Hash` type, and constants path. The
// DB/staging-coupled test module is dropped (this crate has its own tests).

//! Public reachability query and insertion API.

use crate::constants;
use crate::interval::Interval;
use crate::store::{ReachabilityStore, ReachabilityStoreReader};
use crate::tree::{add_tree_block, try_advancing_reindex_root};
use crate::{blockhash, Hash, ReachabilityError, Result};

/// Initialize the reachability store (idempotent).
pub fn init(store: &mut (impl ReachabilityStore + ?Sized)) -> Result<()> {
    init_with_params(store, blockhash::ORIGIN, Interval::maximal())
}

pub(crate) fn init_with_params(
    store: &mut (impl ReachabilityStore + ?Sized),
    origin: Hash,
    capacity: Interval,
) -> Result<()> {
    if store.has(origin)? {
        return Ok(());
    }
    store.init(origin, capacity)?;
    Ok(())
}

type HashIterator<'a> = &'a mut dyn Iterator<Item = Hash>;

/// Add a block to the reachability structures (`selected_parent` is its tree
/// parent; `mergeset_iterator` yields the off-selected-chain blocks it merges).
pub fn add_block(
    store: &mut (impl ReachabilityStore + ?Sized),
    new_block: Hash,
    selected_parent: Hash,
    mergeset_iterator: HashIterator,
) -> Result<()> {
    add_block_with_params(
        store,
        new_block,
        selected_parent,
        mergeset_iterator,
        None,
        None,
    )
}

fn add_block_with_params(
    store: &mut (impl ReachabilityStore + ?Sized),
    new_block: Hash,
    selected_parent: Hash,
    mergeset_iterator: HashIterator,
    reindex_depth: Option<u64>,
    reindex_slack: Option<u64>,
) -> Result<()> {
    add_tree_block(
        store,
        new_block,
        selected_parent,
        reindex_depth.unwrap_or(constants::perf::DEFAULT_REINDEX_DEPTH),
        reindex_slack.unwrap_or(constants::perf::DEFAULT_REINDEX_SLACK),
    )?;
    add_dag_block(store, new_block, mergeset_iterator)?;
    Ok(())
}

fn add_dag_block(
    store: &mut (impl ReachabilityStore + ?Sized),
    new_block: Hash,
    mergeset_iterator: HashIterator,
) -> Result<()> {
    for merged_block in mergeset_iterator {
        insert_to_future_covering_set(store, merged_block, new_block)?;
    }
    Ok(())
}

/// Permanently delete a block while keeping reachability info for all others.
pub fn delete_block(
    store: &mut (impl ReachabilityStore + ?Sized),
    block: Hash,
    mergeset_iterator: HashIterator,
) -> Result<()> {
    let interval = store.get_interval(block)?;
    let parent = store.get_parent(block)?;
    let children = store.get_children(block)?;

    let block_index =
        match binary_search_descendant(store, store.get_children(parent)?.as_slice(), block)? {
            SearchOutput::NotFound(_) => return Err(ReachabilityError::DataInconsistency),
            SearchOutput::Found(hash, i) => {
                debug_assert_eq!(hash, block);
                i
            }
        };

    store.replace_child(parent, block, block_index, &children)?;

    for child in children.iter().copied() {
        store.set_parent(child, parent)?;
    }

    for merged_block in mergeset_iterator {
        match binary_search_descendant(
            store,
            store.get_future_covering_set(merged_block)?.as_slice(),
            block,
        )? {
            SearchOutput::NotFound(_) => return Err(ReachabilityError::DataInconsistency),
            SearchOutput::Found(hash, i) => {
                debug_assert_eq!(hash, block);
                store.replace_future_covering_item(merged_block, block, i, &children)?;
            }
        }
    }

    match children.len() {
        0 => {
            if block_index > 0 {
                let sibling = store.get_children(parent)?[block_index - 1];
                let sibling_interval = store.get_interval(sibling)?;
                store.set_interval(sibling, Interval::new(sibling_interval.start, interval.end))?;
            }
        }
        1 => {
            store.set_interval(children[0], interval)?;
        }
        _ => {
            let first_child = children[0];
            let first_interval = store.get_interval(first_child)?;
            store.set_interval(
                first_child,
                Interval::new(interval.start, first_interval.end),
            )?;

            let last_child = children.last().copied().expect("len > 1");
            let last_interval = store.get_interval(last_child)?;
            store.set_interval(last_child, Interval::new(last_interval.start, interval.end))?;
        }
    }

    store.delete(block)?;
    Ok(())
}

fn insert_to_future_covering_set(
    store: &mut (impl ReachabilityStore + ?Sized),
    merged_block: Hash,
    new_block: Hash,
) -> Result<()> {
    match binary_search_descendant(
        store,
        store.get_future_covering_set(merged_block)?.as_slice(),
        new_block,
    )? {
        SearchOutput::Found(_, _) => Err(ReachabilityError::DataInconsistency),
        SearchOutput::NotFound(i) => {
            store.insert_future_covering_item(merged_block, new_block, i)?;
            Ok(())
        }
    }
}

/// Hint that `hint` is a candidate to become the virtual selected parent; this
/// may advance the internal reindex root.
pub fn hint_virtual_selected_parent(
    store: &mut (impl ReachabilityStore + ?Sized),
    hint: Hash,
) -> Result<()> {
    try_advancing_reindex_root(
        store,
        hint,
        constants::perf::DEFAULT_REINDEX_DEPTH,
        constants::perf::DEFAULT_REINDEX_SLACK,
    )
}

/// Is `this` a strict chain ancestor of `queried` (`this ∈ chain(queried)`, `this != queried`)?
pub fn is_strict_chain_ancestor_of(
    store: &(impl ReachabilityStoreReader + ?Sized),
    this: Hash,
    queried: Hash,
) -> Result<bool> {
    Ok(store
        .get_interval(this)?
        .strictly_contains(store.get_interval(queried)?))
}

/// Is `this` a chain ancestor of `queried` (`this ∈ chain(queried) ∪ {queried}`)? O(1).
pub fn is_chain_ancestor_of(
    store: &(impl ReachabilityStoreReader + ?Sized),
    this: Hash,
    queried: Hash,
) -> Result<bool> {
    Ok(store
        .get_interval(this)?
        .contains(store.get_interval(queried)?))
}

/// Is `this` a DAG ancestor of `queried` (`queried ∈ future(this) ∪ {this}`)?
/// O(log(|future_covering_set(this)|)).
pub fn is_dag_ancestor_of(
    store: &(impl ReachabilityStoreReader + ?Sized),
    this: Hash,
    queried: Hash,
) -> Result<bool> {
    if is_chain_ancestor_of(store, this, queried)? {
        return Ok(true);
    }
    match binary_search_descendant(
        store,
        store.get_future_covering_set(this)?.as_slice(),
        queried,
    )? {
        SearchOutput::Found(_, _) => Ok(true),
        SearchOutput::NotFound(_) => Ok(false),
    }
}

/// The tree child of `ancestor` which is also a chain ancestor of `descendant`.
pub fn get_next_chain_ancestor(
    store: &(impl ReachabilityStoreReader + ?Sized),
    descendant: Hash,
    ancestor: Hash,
) -> Result<Hash> {
    if descendant == ancestor {
        return Err(ReachabilityError::BadQuery);
    }
    if !is_strict_chain_ancestor_of(store, ancestor, descendant)? {
        return Err(ReachabilityError::BadQuery);
    }
    get_next_chain_ancestor_unchecked(store, descendant, ancestor)
}

/// Unchecked variant for internal use (during reindex an `ancestor` interval may
/// not be propagated yet).
pub(crate) fn get_next_chain_ancestor_unchecked(
    store: &(impl ReachabilityStoreReader + ?Sized),
    descendant: Hash,
    ancestor: Hash,
) -> Result<Hash> {
    match binary_search_descendant(store, store.get_children(ancestor)?.as_slice(), descendant)? {
        SearchOutput::Found(hash, _) => Ok(hash),
        SearchOutput::NotFound(_) => Err(ReachabilityError::BadQuery),
    }
}

enum SearchOutput {
    NotFound(usize),
    Found(Hash, usize),
}

fn binary_search_descendant(
    store: &(impl ReachabilityStoreReader + ?Sized),
    ordered_hashes: &[Hash],
    descendant: Hash,
) -> Result<SearchOutput> {
    if cfg!(debug_assertions) {
        assert_hashes_ordered(store, ordered_hashes);
    }

    let point = store.get_interval(descendant)?.end;

    match ordered_hashes.binary_search_by_key(&point, |c| store.get_interval(*c).unwrap().start) {
        Ok(i) => Ok(SearchOutput::Found(ordered_hashes[i], i)),
        Err(i) => {
            if i > 0 && is_chain_ancestor_of(store, ordered_hashes[i - 1], descendant)? {
                Ok(SearchOutput::Found(ordered_hashes[i - 1], i - 1))
            } else {
                Ok(SearchOutput::NotFound(i))
            }
        }
    }
}

fn assert_hashes_ordered(store: &(impl ReachabilityStoreReader + ?Sized), ordered_hashes: &[Hash]) {
    let intervals: Vec<Interval> = ordered_hashes
        .iter()
        .cloned()
        .map(|c| store.get_interval(c).unwrap())
        .collect();
    debug_assert!(intervals
        .as_slice()
        .windows(2)
        .all(|w| w[0].end < w[1].start))
}
