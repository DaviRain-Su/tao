//! Recent-blockhash tracking.
//!
//! Solana transactions reference a *recent blockhash* for replay protection and
//! expiry: a transaction is valid only while its blockhash is still within the
//! last `max_age` blocks. This queue records recent block hashes so the RPC can
//! serve `getLatestBlockhash` and the runtime can reject expired transactions.

use std::collections::VecDeque;

use solana_hash::Hash;

/// A bounded queue of recent block hashes (newest at the front).
#[derive(Debug, Clone)]
pub struct BlockhashQueue {
    recent: VecDeque<Hash>,
    max_age: usize,
}

impl BlockhashQueue {
    /// Track at most `max_age` recent hashes (Solana mainnet uses 150).
    pub fn new(max_age: usize) -> Self {
        assert!(max_age > 0, "max_age must be positive");
        Self {
            recent: VecDeque::with_capacity(max_age),
            max_age,
        }
    }

    /// Record a newly accepted block hash, evicting the oldest beyond `max_age`.
    pub fn register(&mut self, hash: Hash) {
        self.recent.push_front(hash);
        while self.recent.len() > self.max_age {
            self.recent.pop_back();
        }
    }

    /// Is `hash` recent enough to be a valid transaction blockhash?
    pub fn contains(&self, hash: &Hash) -> bool {
        self.recent.iter().any(|h| h == hash)
    }

    /// The most recent block hash, if any.
    pub fn latest(&self) -> Option<Hash> {
        self.recent.front().cloned()
    }

    /// Age of `hash` in blocks (0 = latest), or `None` if not present/expired.
    pub fn age_of(&self, hash: &Hash) -> Option<usize> {
        self.recent.iter().position(|h| h == hash)
    }

    pub fn len(&self) -> usize {
        self.recent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash::new_from_array([n; 32])
    }

    #[test]
    fn tracks_latest_and_membership() {
        let mut q = BlockhashQueue::new(3);
        assert!(q.latest().is_none());
        q.register(h(1));
        q.register(h(2));
        assert_eq!(q.latest(), Some(h(2)));
        assert!(q.contains(&h(1)));
        assert_eq!(q.age_of(&h(2)), Some(0));
        assert_eq!(q.age_of(&h(1)), Some(1));
    }

    #[test]
    fn evicts_beyond_max_age() {
        let mut q = BlockhashQueue::new(2);
        q.register(h(1));
        q.register(h(2));
        q.register(h(3));
        assert_eq!(q.len(), 2);
        assert!(!q.contains(&h(1))); // evicted
        assert!(q.contains(&h(2)));
        assert!(q.contains(&h(3)));
    }
}
