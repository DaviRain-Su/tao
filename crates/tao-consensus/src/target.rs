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
    if t.is_zero() {
        return U256::MAX;
    }
    // (!t) == 2^256 - 1 - t
    let numerator = !t;
    let denom = t.saturating_add(U256::one());
    numerator / denom + U256::one()
}

/// Number of leading zero bits of a 256-bit big-endian value.
fn leading_zero_bits(x: &[u8; 32]) -> u32 {
    let mut z = 0u32;
    for b in x {
        if *b == 0 {
            z += 8;
        } else {
            z += b.leading_zeros();
            break;
        }
    }
    z
}

/// The PoW **level** of a solved block: how many times harder its actual PoW hash
/// is than the required target, as a power of two. `level = k` means
/// `hash <= target / 2^k`, i.e. the hash cleared `k` more leading zero bits than
/// the target demanded — a 2^k-rarer event.
///
/// This is the building block of succinct PoW proofs (NiPoPoW / Kaspa's pruning
/// proof): high-level blocks are rare and sample the chain's accumulated work, so
/// a short chain of them certifies a lot of work without the full history.
///
/// Guarded against invalid solutions: returns 0 when `hash > target`.
pub fn pow_level(pow_hash: &[u8; 32], target: &Target) -> u32 {
    if !meets_target(pow_hash, target) {
        return 0;
    }
    leading_zero_bits(pow_hash).saturating_sub(leading_zero_bits(target))
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
        let t = target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
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

    #[test]
    fn pow_level_counts_extra_zero_bits() {
        // target allows hashes < 2^248 (one leading zero byte).
        let target =
            target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        // A hash that just meets the target → level 0.
        let at_target =
            target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_eq!(pow_level(&at_target, &target), 0);
        // One extra zero byte (8 more leading zero bits) → level 8.
        let harder =
            target_from_hex("0000ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_eq!(pow_level(&harder, &target), 8);
        // All-zero hash is maximally rare.
        assert_eq!(pow_level(&[0u8; 32], &target), 256 - 8);
    }

    #[test]
    fn pow_level_is_one_for_double_difficulty() {
        let target =
            target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        // 0x007f.. has one more leading zero bit than 0x00ff.. → level 1.
        let one_bit_harder =
            target_from_hex("007fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_eq!(pow_level(&one_bit_harder, &target), 1);
    }

    #[test]
    fn work_for_zero_target_is_max() {
        assert_eq!(work_for_target(&[0u8; 32]), U256::MAX);
    }

    #[test]
    fn pow_level_zero_if_not_meeting_target() {
        let target =
            target_from_hex("00ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        // One larger than target, so not a valid PoW solution.
        let invalid =
            target_from_hex("01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_eq!(pow_level(&invalid, &target), 0);
    }
}
