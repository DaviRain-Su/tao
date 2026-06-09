//! Apply genesis allocations into the account store.
//!
//! Each [`Allocation`](tao_core::genesis::Allocation) becomes a System-owned
//! account (a normal wallet) funded with the configured lamports. Loading is
//! idempotent: an account that already exists (e.g. after a restart) is left
//! untouched so balances are not reset.

use std::str::FromStr;

use solana_account::AccountSharedData;
use solana_pubkey::Pubkey;
use tao_core::genesis::GenesisConfig;
use tao_database::AccountsDb;

/// Result of applying genesis allocations.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GenesisLoad {
    /// Accounts newly created.
    pub created: usize,
    /// Accounts skipped because they already existed.
    pub skipped: usize,
}

/// Apply `genesis.allocations` to `db`. Idempotent across restarts.
pub fn load_allocations(genesis: &GenesisConfig, db: &AccountsDb) -> Result<GenesisLoad, String> {
    let system_owner = solana_sdk_ids::system_program::id();
    let mut load = GenesisLoad::default();

    for alloc in &genesis.allocations {
        let pubkey = Pubkey::from_str(&alloc.address)
            .map_err(|e| format!("invalid allocation address '{}': {e}", alloc.address))?;

        let exists = db.get(&pubkey).map_err(|e| e.to_string())?.is_some();
        if exists {
            load.skipped += 1;
            continue;
        }
        let account = AccountSharedData::new(alloc.lamports, 0, &system_owner);
        db.set(&pubkey, &account).map_err(|e| e.to_string())?;
        load.created += 1;
    }
    Ok(load)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_account::ReadableAccount;
    use tao_core::genesis::{Allocation, GenesisConfig};

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tao-runtime-genesis-{}-{tag}", std::process::id()))
    }

    fn genesis_with(allocs: Vec<Allocation>) -> GenesisConfig {
        let mut g = GenesisConfig::devnet();
        g.allocations = allocs;
        g
    }

    #[test]
    fn applies_allocations_and_is_idempotent() {
        let dir = tmp_dir("alloc");
        let _ = std::fs::remove_dir_all(&dir);
        let db = AccountsDb::open(&dir).unwrap();

        let addr = Pubkey::new_unique();
        let genesis = genesis_with(vec![Allocation {
            address: addr.to_string(),
            lamports: 5_000_000,
        }]);

        let first = load_allocations(&genesis, &db).unwrap();
        assert_eq!(first, GenesisLoad { created: 1, skipped: 0 });
        assert_eq!(db.get(&addr).unwrap().unwrap().lamports(), 5_000_000);

        // Second run skips the existing account (no balance reset).
        let second = load_allocations(&genesis, &db).unwrap();
        assert_eq!(second, GenesisLoad { created: 0, skipped: 1 });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_address() {
        let dir = tmp_dir("bad");
        let _ = std::fs::remove_dir_all(&dir);
        let db = AccountsDb::open(&dir).unwrap();
        let genesis = genesis_with(vec![Allocation {
            address: "not-base58!!".to_string(),
            lamports: 1,
        }]);
        assert!(load_allocations(&genesis, &db).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
