//! Build the genesis block header from a [`GenesisConfig`].

use tao_core::genesis::GenesisConfig;
use tao_core::{Hash, Pubkey};

use crate::block::{BlockHeader, HEADER_VERSION};
use crate::target::Target;

/// Parse a hex-encoded 256-bit target into 32 bytes.
pub fn parse_target(hex_str: &str) -> Result<Target, String> {
    let bytes = hex::decode(hex_str).map_err(|e| format!("invalid target hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("target must be 32 bytes, got {}", bytes.len()));
    }
    let mut t = [0u8; 32];
    t.copy_from_slice(&bytes);
    Ok(t)
}

/// Construct the deterministic genesis header for a network.
///
/// Genesis carries no PoW (its nonce is fixed at 0); all nodes derive the same
/// genesis id from the same [`GenesisConfig`]. The full config is committed via
/// its hash in `state_root`, so configs differing in any consensus parameter
/// (allocations, reward, PoW algorithm, …) yield different genesis ids — nodes
/// with mismatched genesis files cannot silently share a chain.
pub fn genesis_header(cfg: &GenesisConfig) -> Result<BlockHeader, String> {
    let target = parse_target(&cfg.pow.initial_target)?;
    Ok(BlockHeader {
        version: HEADER_VERSION,
        prev_hash: Hash::default(),
        height: 0,
        timestamp: cfg.creation_time,
        tx_merkle_root: Hash::default(),
        state_root: Hash::new_from_array(cfg.commitment()),
        target,
        nonce: 0,
        miner: Pubkey::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devnet_genesis_is_deterministic() {
        let cfg = GenesisConfig::devnet();
        let a = genesis_header(&cfg).unwrap();
        let b = genesis_header(&cfg).unwrap();
        assert_eq!(a.id(), b.id());
        assert!(a.is_genesis());
    }

    #[test]
    fn genesis_id_commits_to_the_config() {
        let base = GenesisConfig::devnet();
        let mut tampered = base.clone();
        tampered.reward.initial_lamports += 1;
        assert_ne!(
            genesis_header(&base).unwrap().id(),
            genesis_header(&tampered).unwrap().id(),
            "a consensus-parameter change must change the genesis id"
        );
    }

    #[test]
    fn rejects_bad_target() {
        assert!(parse_target("00ff").is_err());
        assert!(parse_target("zz").is_err());
    }
}
