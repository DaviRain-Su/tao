//! RocksDB-backed account store.
//!
//! Maps a Solana-compatible [`Pubkey`] to an [`AccountSharedData`]. This is the
//! mutable world state the SVM executes against (M2). It also computes a
//! deterministic `state_root` so independent nodes can confirm they reached the
//! same post-execution state.

use std::path::Path;

use rocksdb::{IteratorMode, Options, WriteBatch, DB};
use solana_account::{AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;
use tao_core::error::{Result, TaoError};

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
    /// Open (creating if absent) the account store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, path).map_err(storage_err)?;
        Ok(Self { db })
    }

    /// Fetch an account, if present.
    pub fn get(&self, key: &Pubkey) -> Result<Option<AccountSharedData>> {
        match self.db.get(key.as_ref()).map_err(storage_err)? {
            Some(bytes) => Ok(Some(bincode::deserialize(&bytes).map_err(ser_err)?)),
            None => Ok(None),
        }
    }

    /// Insert or overwrite a single account.
    pub fn set(&self, key: &Pubkey, account: &AccountSharedData) -> Result<()> {
        let bytes = bincode::serialize(account).map_err(ser_err)?;
        self.db.put(key.as_ref(), bytes).map_err(storage_err)
    }

    /// Remove an account.
    pub fn delete(&self, key: &Pubkey) -> Result<()> {
        self.db.delete(key.as_ref()).map_err(storage_err)
    }

    /// Atomically apply a set of account changes. Accounts that end up with zero
    /// lamports and no data are purged (Solana semantics).
    pub fn commit(
        &self,
        changes: impl IntoIterator<Item = (Pubkey, AccountSharedData)>,
    ) -> Result<()> {
        let mut batch = WriteBatch::default();
        for (key, account) in changes {
            if account.lamports() == 0 && account.data().is_empty() {
                batch.delete(key.as_ref());
            } else {
                let bytes = bincode::serialize(&account).map_err(ser_err)?;
                batch.put(key.as_ref(), bytes);
            }
        }
        self.db.write(batch).map_err(storage_err)
    }

    /// Deterministic hash over the entire account set.
    ///
    /// RocksDB iterates in lexicographic key order, so the digest is independent
    /// of insertion order — two nodes with the same accounts get the same root.
    /// (A Sparse Merkle Tree replaces this full scan later; see the plan.)
    pub fn state_root(&self) -> Result<[u8; 32]> {
        let mut hasher = blake3::Hasher::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (k, v) = item.map_err(storage_err)?;
            hasher.update(&(k.len() as u32).to_be_bytes());
            hasher.update(&k);
            hasher.update(&(v.len() as u32).to_be_bytes());
            hasher.update(&v);
        }
        Ok(*hasher.finalize().as_bytes())
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
