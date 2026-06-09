//! PoW target and chain-work arithmetic.
//!
//! A block's PoW *target* is a 256-bit threshold stored big-endian in the
//! header. A candidate is valid when its PoW hash, interpreted as a 256-bit
//! big-endian integer, is `<=` the target. Lower target = harder.
//!
//! *Work* (a.k.a. chainwork) is the expected number of hashes to beat a target,
//! `work(t) = 2^256 / (t + 1)`. Cumulative work across the chain is what the
//! fork-choice rule maximizes (Bitcoin's "most work" rule).

use primitive_types::U256;

/// A 256-bit PoW target threshold (big-endian bytes).
pub type Target = [u8; 32];

/// Interpret 32 big-endian bytes as a U256.
pub fn to_u256(bytes: &[u8; 32]) -> U256 {
    U256::from_big_endian(bytes)
}

/// Serialize a U256 to 32 big-endian bytes.
pub fn from_u256(value: U256) -> [u8; 32] {
    value.to_big_endian()
}

/// Does `hash` (big-endian) meet `target` (i.e. `hash <= target`)?
pub fn meets_target(hash: &[u8; 32], target: &Target) -> bool {
    to_u256(hash) <= to_u256(target)
}

/// Expected work to solve a block at `target`: `2^256 / (target + 1)`.
///
/// Computed as `(2^256 - 1 - target) / (target + 1) + 1` to stay within 256
/// bits (this is exactly Bitcoin's `GetBlockProof`).
pub fn work_for_target(target: &Target) -> U256 {
    let t = to_u256(target);
    // (!t) == 2^256 - 1 - t
    let numerator = !t;
    let denom = t.saturating_add(U256::one());
    numerator / denom + U256::one()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_from_hex(hex: &str) -> Target {
        let bytes = hex::decode(hex).unwrap();
        let mut t = [0u8; 32];
        t.copy_from_slice(&bytes);
        t
    }

    #[test]
    fn roundtrip_u256() {
        let t = target_from_hex(
            "00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        );
        assert_eq!(from_u256(to_u256(&t)), t);
    }

    #[test]
    fn meets_target_basic() {
        let target =
            target_from_hex("0000ff00000000000000000000000000000000000000000000000000000000ff");
        let low = [0u8; 32];
        assert!(meets_target(&low, &target));
        let mut high = [0u8; 32];
        high[0] = 0x01; // 0x01.. > 0x0000ff..
        assert!(!meets_target(&high, &target));
    }

    #[test]
    fn easier_target_means_less_work() {
        let easy =
            target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        let hard =
            target_from_hex("0000ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert!(work_for_target(&hard) > work_for_target(&easy));
    }
}
