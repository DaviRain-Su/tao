//! RocksDB-backed account store.
//!
//! Maps a Solana-compatible [`Pubkey`] to an [`AccountSharedData`]. This is the
//! mutable world state the SVM executes against (M2). It also computes a
//! deterministic `state_root` so independent nodes can confirm they reached the
//! same post-execution state.

use std::collections::HashMap;
use std::path::Path;

use rocksdb::{ColumnFamily, IteratorMode, Options, WriteBatch, DB};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;
use tao_core::error::{Result, TaoError};

use crate::smt;

/// Column family holding the persistent Sparse Merkle Tree nodes (accounts live
/// in the default CF).
const SMT_CF: &str = "smt";

fn storage_err<E: std::fmt::Display>(e: E) -> TaoError {
    TaoError::Storage(e.to_string())
}

fn ser_err<E: std::fmt::Display>(e: E) -> TaoError {
    TaoError::Serialization(e.to_string())
}

/// A persistent account store.
pub struct AccountsDb {
    db: DB,
}

impl AccountsDb {
    /// Open (creating if absent) the account store at `path`, with the SMT column
    /// family. (The account store is a derived cache rebuilt from the block log on
    /// startup, so the SMT is rebuilt incrementally alongside — no migration.)
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let db = DB::open_cf(&opts, path, [SMT_CF]).map_err(storage_err)?;
        Ok(Self { db })
    }

    fn smt_cf(&self) -> &ColumnFamily {
        self.db.cf_handle(SMT_CF).expect("smt column family")
    }

    /// Fetch an account, if present.
    pub fn get(&self, key: &Pubkey) -> Result<Option<AccountSharedData>> {
        match self.db.get(key.as_ref()).map_err(storage_err)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes).map_err(ser_err)?)),
            None => Ok(None),
        }
    }

    /// The SMT leaf value committed for an account (or `EMPTY_LEAF` if absent).
    fn leaf_for(key: &Pubkey, bytes: Option<&[u8]>) -> [u8; 32] {
        match bytes {
            Some(b) => smt::leaf_hash(&key.to_bytes(), &Self::account_value_hash(b)),
            None => smt::EMPTY_LEAF,
        }
    }

    /// Apply SMT leaf updates into `batch` (atomic with the account writes). A
    /// single overlay is used so updates within one call see each other's path
    /// changes (correct when keys share path prefixes).
    fn smt_update(&self, batch: &mut WriteBatch, leaves: &[([u8; 32], [u8; 32])]) {
        let cf = self.smt_cf();
        let mut overlay = Overlay { db: &self.db, cf, pending: HashMap::new() };
        for (key, leaf) in leaves {
            smt::update_leaf(&mut overlay, key, *leaf);
        }
        for (id, v) in overlay.pending {
            match v {
                Some(h) => batch.put_cf(cf, &id, h),
                None => batch.delete_cf(cf, &id),
            }
        }
    }

    /// Insert or overwrite a single account (updates the SMT atomically).
    pub fn set(&self, key: &Pubkey, account: &AccountSharedData) -> Result<()> {
        let bytes = bincode::serialize(account).map_err(ser_err)?;
        let leaf = Self::leaf_for(key, Some(&bytes));
        let mut batch = WriteBatch::default();
        batch.put(key.as_ref(), &bytes);
        self.smt_update(&mut batch, &[(key.to_bytes(), leaf)]);
        self.db.write(batch).map_err(storage_err)
    }

    /// Remove an account (updates the SMT atomically).
    pub fn delete(&self, key: &Pubkey) -> Result<()> {
        let mut batch = WriteBatch::default();
        batch.delete(key.as_ref());
        self.smt_update(&mut batch, &[(key.to_bytes(), smt::EMPTY_LEAF)]);
        self.db.write(batch).map_err(storage_err)
    }

    /// Atomically apply a set of account changes (and their SMT updates).
    /// Accounts that end up with zero lamports and no data are purged (Solana
    /// semantics).
    pub fn commit(
        &self,
        changes: impl IntoIterator<Item = (Pubkey, AccountSharedData)>,
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        let mut leaves = Vec::new();
        for (key, account) in changes {
            if account.lamports() == 0 && account.data().is_empty() {
                batch.delete(key.as_ref());
                leaves.push((key.to_bytes(), smt::EMPTY_LEAF));
            } else {
                let bytes = bincode::serialize(&account).map_err(ser_err)?;
                leaves.push((key.to_bytes(), Self::leaf_for(&key, Some(&bytes))));
                batch.put(key.as_ref(), bytes);
            }
        }
        self.smt_update(&mut batch, &leaves);
        self.db.write(batch).map_err(storage_err)
    }

    /// Value hash of a stored account: BLAKE3 of its serialized bytes. The SMT
    /// leaf commits `(key, value_hash)`. A light client recomputes this from the
    /// account it received.
    pub fn account_value_hash(account_bytes: &[u8]) -> [u8; 32] {
        *blake3::hash(account_bytes).as_bytes()
    }

    /// The **Sparse Merkle Tree root** over the account set: a 256-bit commitment
    /// a light client can verify individual accounts against (see
    /// [`state_proof`](Self::state_proof)). Maintained incrementally in storage,
    /// so this is an O(1) lookup. Deterministic — two nodes with the same accounts
    /// agree (independent of insertion order).
    pub fn state_root(&self) -> Result<[u8; 32]> {
        let cf = self.smt_cf();
        let store = Overlay { db: &self.db, cf, pending: HashMap::new() };
        Ok(smt::stored_root(&store))
    }

    /// A light-client proof for `key`: the account (or `None` if absent) plus its
    /// SMT inclusion/exclusion proof against [`state_root`](Self::state_root). The
    /// client verifies with [`crate::smt::verify`] using
    /// `leaf_hash(key, account_value_hash(account))` (present) or `EMPTY_LEAF`
    /// (absent).
    pub fn state_proof(
        &self,
        key: &Pubkey,
    ) -> Result<(Option<AccountSharedData>, smt::MerkleProof)> {
        let cf = self.smt_cf();
        let store = Overlay { db: &self.db, cf, pending: HashMap::new() };
        let proof = smt::stored_proof(&store, &key.to_bytes());
        Ok((self.get(key)?, proof))
    }

    /// Dump every account as (pubkey, account) pairs — for snapshotting state
    /// (e.g. a finalized checkpoint) so it can be restored without re-executing
    /// history.
    pub fn dump(&self) -> Result<Vec<(Pubkey, AccountSharedData)>> {
        let mut out = Vec::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (k, v) = item.map_err(storage_err)?;
            let key = Pubkey::try_from(k.as_ref()).map_err(|_| storage_err("bad pubkey key"))?;
            let account: AccountSharedData = bincode::deserialize(&v).map_err(ser_err)?;
            out.push((key, account));
        }
        Ok(out)
    }

    /// Number of stored accounts.
    pub fn len(&self) -> Result<usize> {
        let mut n = 0;
        for item in self.db.iterator(IteratorMode::Start) {
            item.map_err(storage_err)?;
            n += 1;
        }
        Ok(n)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.db.iterator(IteratorMode::Start).next().is_none())
    }
}

/// A [`smt::NodeStore`] over the SMT column family with a write overlay: reads
/// check `pending` first (so updates within one commit see each other), then fall
/// through to RocksDB; writes accumulate in `pending` to be flushed to a
/// `WriteBatch` by the caller (atomic with the account writes).
struct Overlay<'a> {
    db: &'a DB,
    cf: &'a ColumnFamily,
    pending: HashMap<Vec<u8>, Option<[u8; 32]>>,
}

impl smt::NodeStore for Overlay<'_> {
    fn get_node(&self, id: &[u8]) -> Option<[u8; 32]> {
        if let Some(v) = self.pending.get(id) {
            return *v;
        }
        let bytes = self.db.get_cf(self.cf, id).ok().flatten()?;
        (bytes.len() == 32).then(|| {
            let mut a = [0u8; 32];
            a.copy_from_slice(&bytes);
            a
        })
    }
    fn set_node(&mut self, id: &[u8], hash: [u8; 32]) {
        self.pending.insert(id.to_vec(), Some(hash));
    }
    fn del_node(&mut self, id: &[u8]) {
        self.pending.insert(id.to_vec(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tao-accountsdb-{}-{tag}", std::process::id()))
    }

    fn account(lamports: u64, owner: Pubkey) -> AccountSharedData {
        AccountSharedData::new(lamports, 0, &owner)
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let dir = tmp_dir("rt");
        let _ = std::fs::remove_dir_all(&dir);
        let db = AccountsDb::open(&dir).unwrap();

        let key = Pubkey::new_unique();
        assert!(db.get(&key).unwrap().is_none());

        let acct = account(1_000, Pubkey::new_unique());
        db.set(&key, &acct).unwrap();
        let got = db.get(&key).unwrap().unwrap();
        assert_eq!(got.lamports(), 1_000);

        db.delete(&key).unwrap();
        assert!(db.get(&key).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn light_client_proof_against_state_root() {
        let dir = tmp_dir("proof");
        let _ = std::fs::remove_dir_all(&dir);
        let db = AccountsDb::open(&dir).unwrap();
        let owner = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        db.set(&a, &account(111, owner)).unwrap();
        db.set(&b, &account(222, owner)).unwrap();

        let root = db.state_root().unwrap();

        // Inclusion: a light client verifies account `a` against the root.
        let (acct, proof) = db.state_proof(&a).unwrap();
        let acct = acct.expect("present");
        let bytes = bincode::serialize(&acct).unwrap();
        let leaf = crate::smt::leaf_hash(&a.to_bytes(), &AccountsDb::account_value_hash(&bytes));
        assert!(crate::smt::verify(&root, &a.to_bytes(), &leaf, &proof));
        assert_eq!(acct.lamports(), 111);

        // Exclusion: an absent key verifies against EMPTY_LEAF, not any value.
        let absent = Pubkey::new_unique();
        let (none, xproof) = db.state_proof(&absent).unwrap();
        assert!(none.is_none());
        assert!(crate::smt::verify(&root, &absent.to_bytes(), &crate::smt::EMPTY_LEAF, &xproof));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_root_is_order_independent_and_change_sensitive() {
        let dir1 = tmp_dir("sr1");
        let dir2 = tmp_dir("sr2");
        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);

        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        // Same accounts inserted in opposite orders → identical state root.
        let db1 = AccountsDb::open(&dir1).unwrap();
        db1.set(&a, &account(1, owner)).unwrap();
        db1.set(&b, &account(2, owner)).unwrap();

        let db2 = AccountsDb::open(&dir2).unwrap();
        db2.set(&b, &account(2, owner)).unwrap();
        db2.set(&a, &account(1, owner)).unwrap();

        assert_eq!(db1.state_root().unwrap(), db2.state_root().unwrap());

        // Changing a balance changes the root.
        db2.set(&a, &account(999, owner)).unwrap();
        assert_ne!(db1.state_root().unwrap(), db2.state_root().unwrap());

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn commit_purges_empty_accounts() {
        let dir = tmp_dir("purge");
        let _ = std::fs::remove_dir_all(&dir);
        let db = AccountsDb::open(&dir).unwrap();

        let key = Pubkey::new_unique();
        db.set(&key, &account(5, Pubkey::new_unique())).unwrap();
        assert_eq!(db.len().unwrap(), 1);

        // Zero-lamport, empty-data account is purged on commit.
        db.commit([(key, AccountSharedData::new(0, 0, &Pubkey::default()))])
            .unwrap();
        assert!(db.get(&key).unwrap().is_none());
        assert_eq!(db.len().unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
