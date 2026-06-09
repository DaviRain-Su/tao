//! `tao-mempool` — the transaction pool and block-template construction.
//!
//! Current MVP: simple FIFO pool with signature deduplication and size cap.
//! No advanced fee prioritization yet (Solana compute budget priority fees
//! can be added later by inspecting instructions).
//!
//! Used by the miner to build block templates. Especially relevant when
//! PoW is expensive (e.g. matmul-PoUW) and blocks are produced less frequently.

use std::collections::HashSet;

use solana_transaction::Transaction;

pub use tao_core::Result;

/// A very basic in-memory mempool.
#[derive(Default)]
pub struct Mempool {
    txs: Vec<Transaction>,
    /// First signature of each tx we've seen (for dedup).
    seen: HashSet<[u8; 64]>,
    max_size: usize,
}

impl Mempool {
    /// Create a mempool with a maximum number of pending transactions.
    pub fn new(max_size: usize) -> Self {
        Self {
            txs: Vec::new(),
            seen: HashSet::new(),
            max_size,
        }
    }

    /// Submit a transaction. Returns true if it was added (not a duplicate
    /// and pool was not full).
    pub fn submit(&mut self, tx: Transaction) -> bool {
        if let Some(sig) = tx.signatures.first().and_then(|s| TryInto::<[u8; 64]>::try_into(s.as_ref()).ok()) {
            if self.seen.contains(&sig) {
                return false;
            }
            if self.txs.len() >= self.max_size {
                // Simple eviction: drop the oldest when full.
                if let Some(old) = self.txs.first() {
                    if let Some(old_sig) = old.signatures.first().and_then(|s| TryInto::<[u8; 64]>::try_into(s.as_ref()).ok()) {
                        self.seen.remove(&old_sig);
                    }
                }
                self.txs.remove(0);
            }
            self.seen.insert(sig);
            self.txs.push(tx);
            true
        } else {
            false
        }
    }

    /// Drain up to `max_count` transactions for inclusion in a new block template.
    /// Returns the transactions (caller is responsible for execution + removal
    /// from confirmed status tracking).
    pub fn drain(&mut self, max_count: usize) -> Vec<Transaction> {
        let count = max_count.min(self.txs.len());
        let drained: Vec<_> = self.txs.drain(0..count).collect();
        for tx in &drained {
            if let Some(sig) = tx.signatures.first().and_then(|s| TryInto::<[u8; 64]>::try_into(s.as_ref()).ok()) {
                self.seen.remove(&sig);
            }
        }
        drained
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }
}
