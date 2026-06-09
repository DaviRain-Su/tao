// Ported from rusty-kaspa, ISC, consensus/src/processes/reachability/extensions.rs.
// Adapted: crate-local module paths and `Hash`/`StoreResult` types.

use crate::interval::Interval;
use crate::store::ReachabilityStoreReader;
use crate::{Hash, StoreResult};

pub(crate) trait ReachabilityStoreIntervalExtensions {
    fn interval_children_capacity(&self, block: Hash) -> StoreResult<Interval>;
    fn interval_remaining_before(&self, block: Hash) -> StoreResult<Interval>;
    fn interval_remaining_after(&self, block: Hash) -> StoreResult<Interval>;
}

impl<T: ReachabilityStoreReader + ?Sized> ReachabilityStoreIntervalExtensions for T {
    /// The reachability allocation capacity for children of `block` (the block's
    /// interval must *strictly* contain its children, hence `decrease_end(1)`).
    fn interval_children_capacity(&self, block: Hash) -> StoreResult<Interval> {
        Ok(self.get_interval(block)?.decrease_end(1))
    }

    /// Available interval to allocate for tree children, from the *beginning* of
    /// the children allocation capacity.
    fn interval_remaining_before(&self, block: Hash) -> StoreResult<Interval> {
        let alloc_capacity = self.interval_children_capacity(block)?;
        match self.get_children(block)?.first() {
            Some(first_child) => {
                let first_alloc = self.get_interval(*first_child)?;
                Ok(Interval::new(alloc_capacity.start, first_alloc.start - 1))
            }
            None => Ok(alloc_capacity),
        }
    }

    /// Available interval to allocate for tree children, from the *end* of the
    /// children allocation capacity.
    fn interval_remaining_after(&self, block: Hash) -> StoreResult<Interval> {
        let alloc_capacity = self.interval_children_capacity(block)?;
        match self.get_children(block)?.last() {
            Some(last_child) => {
                let last_alloc = self.get_interval(*last_child)?;
                Ok(Interval::new(last_alloc.end + 1, alloc_capacity.end))
            }
            None => Ok(alloc_capacity),
        }
    }
}
