//! State shared between the miner thread and the RPC server.
//!
//! The RPC reads account state straight from the shared [`AccountsDb`] (RocksDB
//! is internally synchronized), and submits transactions into a mempool the
//! miner drains. This avoids sharing the non-`Sync` `Bank` across threads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use solana_transaction::Transaction;
use tao_database::AccountsDb;
use tao_mempool::Mempool;
use tao_p2p::{NetMsg, Network};

/// Confirmed transaction status: `None` = success, `Some(err)` = failed.
pub type SigResult = Option<String>;

/// Cross-thread node state.
pub struct Shared {
    /// The account store (also held by the miner's Bank — same instance).
    pub accounts: Arc<AccountsDb>,
    /// Genesis block id (for `getGenesisHash`).
    pub genesis_hash: [u8; 32],
    /// Faucet keypair secret (64 bytes) — enables `requestAirdrop` when set.
    pub faucet: Option<[u8; 64]>,
    slot: AtomicU64,
    latest_blockhash: Mutex<[u8; 32]>,
    mempool: Mutex<Mempool>,
    /// Confirmed signatures → result. Presence means "confirmed".
    sig_status: Mutex<HashMap<[u8; 64], SigResult>>,
    /// Gossip network handle (attached after construction).
    network: OnceLock<Network>,
}

impl Shared {
    pub fn new(
        accounts: Arc<AccountsDb>,
        genesis_hash: [u8; 32],
        slot: u64,
        latest_blockhash: [u8; 32],
        faucet: Option<[u8; 64]>,
    ) -> Self {
        Self {
            accounts,
            genesis_hash,
            faucet,
            slot: AtomicU64::new(slot),
            latest_blockhash: Mutex::new(latest_blockhash),
            mempool: Mutex::new(Mempool::new(4096)), // reasonable cap for prototype
            sig_status: Mutex::new(HashMap::new()),
            network: OnceLock::new(),
        }
    }

    /// Attach the gossip network so the RPC can relay submitted transactions.
    pub fn attach_network(&self, network: Network) {
        let _ = self.network.set(network);
    }

    /// Relay a raw (wire-format) transaction to peers, if networked.
    pub fn gossip_tx(&self, raw: Vec<u8>) {
        if let Some(net) = self.network.get() {
            net.broadcast(&NetMsg::NewTx(raw));
        }
    }

    pub fn slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }

    pub fn latest_blockhash(&self) -> [u8; 32] {
        *self.latest_blockhash.lock().unwrap()
    }

    /// Advance the head: called by the miner after a block is accepted.
    pub fn advance(&self, slot: u64, blockhash: [u8; 32]) {
        self.slot.store(slot, Ordering::Relaxed);
        *self.latest_blockhash.lock().unwrap() = blockhash;
    }

    /// Queue a transaction for inclusion (deduplicated inside the mempool).
    pub fn submit(&self, tx: Transaction) {
        let _ = self.mempool.lock().unwrap().submit(tx);
    }

    /// Drain up to a reasonable number of pending transactions for the next block template.
    /// In slow-PoW (matmul) scenarios this prevents trying to stuff too many txs into one block.
    pub fn drain_mempool(&self) -> Vec<Transaction> {
        self.mempool.lock().unwrap().drain(4096)
    }

    /// Record a confirmed signature result.
    pub fn confirm(&self, signature: [u8; 64], result: SigResult) {
        self.sig_status.lock().unwrap().insert(signature, result);
    }

    /// Look up a signature's confirmation: `None` = unknown, `Some(result)` =
    /// confirmed with that result.
    pub fn signature_status(&self, signature: &[u8; 64]) -> Option<SigResult> {
        self.sig_status.lock().unwrap().get(signature).cloned()
    }
}
