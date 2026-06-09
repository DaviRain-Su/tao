//! Per-block difficulty adjustment using **LWMA** (Linearly Weighted Moving
//! Average, Zawy's LWMA-1).
//!
//! LWMA retargets every block over a short window, weighting recent solve times
//! more heavily. This reacts fast to hashrate swings and resists the
//! rented-hashpower "jump" attacks that Bitcoin's slow 2016-block retarget is
//! vulnerable to — the right choice for a fast chain.
//!
//! We work in *difficulty* (= chain work per block) internally and convert back
//! to a target at the end.

use primitive_types::U256;

use crate::block::BlockHeader;
use crate::target::{from_u256, work_for_target, Target};

/// Difficulty-adjustment parameters.
#[derive(Debug, Clone, Copy)]
pub struct DifficultyParams {
    /// Desired seconds per block (`T`).
    pub target_block_time_secs: u64,
    /// Window length in blocks (`N`).
    pub window: u64,
}

impl DifficultyParams {
    pub fn new(target_block_time_secs: u64, window: u64) -> Self {
        assert!(target_block_time_secs > 0 && window >= 2, "invalid difficulty params");
        Self { target_block_time_secs, window }
    }
}

/// Compute the target for the block that extends `recent`.
///
/// `recent` is the parent chain segment ordered by **ascending height** (so
/// `recent.last()` is the immediate parent). During warmup (fewer than
/// `window + 1` headers) we hold the most recent target, or `genesis_target`
/// if the chain is empty.
pub fn next_target(
    recent: &[BlockHeader],
    params: &DifficultyParams,
    genesis_target: &Target,
) -> Target {
    let n = params.window as usize;
    let t = params.target_block_time_secs as i64;

    if recent.len() <= n {
        return recent.last().map(|h| h.target).unwrap_or(*genesis_target);
    }

    // Take the last N+1 headers → N solve-time intervals.
    let window = &recent[recent.len() - (n + 1)..];
    let max_solvetime = 6 * t;

    let mut weighted_time: i64 = 0; // Σ_{i=1}^N i * solvetime_i
    let mut difficulty_sum = U256::zero(); // Σ work(target_i)
    for i in 1..=n {
        let prev = &window[i - 1];
        let cur = &window[i];
        let solvetime = (cur.timestamp - prev.timestamp).clamp(1, max_solvetime);
        weighted_time += (i as i64) * solvetime;
        difficulty_sum += work_for_target(&cur.target);
    }

    let avg_difficulty = difficulty_sum / U256::from(n as u64);
    // k = T * N(N+1)/2; if blocks arrive exactly on time, weighted_time == k.
    let k = (t as u64) * ((n as u64) * (n as u64 + 1) / 2);
    let weighted_time = weighted_time.max(1) as u64;

    // next_difficulty = avg_difficulty * k / weighted_time
    let next_difficulty = avg_difficulty
        .saturating_mul(U256::from(k))
        / U256::from(weighted_time);

    difficulty_to_target(next_difficulty)
}

/// Convert a difficulty (work) value back into a target threshold.
fn difficulty_to_target(difficulty: U256) -> Target {
    if difficulty.is_zero() {
        return [0xff; 32];
    }
    from_u256(U256::MAX / difficulty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::HEADER_VERSION;
    use tao_core::{Hash, Pubkey};

    fn easy_target() -> Target {
        let mut t = [0xffu8; 32];
        t[0] = 0x00; // ~2^248
        t
    }

    fn chain_with_spacing(n: usize, target: Target, spacing: i64) -> Vec<BlockHeader> {
        (0..n)
            .map(|h| BlockHeader {
                version: HEADER_VERSION,
                prev_hash: Hash::default(),
                height: h as u64,
                timestamp: 1_000_000 + (h as i64) * spacing,
                tx_merkle_root: Hash::default(),
                state_root: Hash::default(),
                target,
                nonce: 0,
                miner: Pubkey::default(),
            })
            .collect()
    }

    #[test]
    fn warmup_holds_target() {
        let params = DifficultyParams::new(10, 90);
        let g = easy_target();
        let chain = chain_with_spacing(10, g, 10);
        assert_eq!(next_target(&chain, &params, &g), g);
    }

    #[test]
    fn on_time_blocks_keep_difficulty_stable() {
        let params = DifficultyParams::new(10, 90);
        let g = easy_target();
        let chain = chain_with_spacing(200, g, 10); // exactly on target
        let nt = next_target(&chain, &params, &g);
        let w_g = work_for_target(&g);
        let w_n = work_for_target(&nt);
        // within ~2%
        assert!(w_n >= w_g * U256::from(98u64) / U256::from(100u64));
        assert!(w_n <= w_g * U256::from(102u64) / U256::from(100u64));
    }

    #[test]
    fn fast_blocks_raise_difficulty() {
        let params = DifficultyParams::new(10, 90);
        let g = easy_target();
        let chain = chain_with_spacing(200, g, 5); // twice as fast
        let nt = next_target(&chain, &params, &g);
        // harder ⇒ more work than the genesis difficulty
        assert!(work_for_target(&nt) > work_for_target(&g));
    }

    #[test]
    fn slow_blocks_lower_difficulty() {
        let params = DifficultyParams::new(10, 90);
        let g = easy_target();
        let chain = chain_with_spacing(200, g, 20); // twice as slow
        let nt = next_target(&chain, &params, &g);
        assert!(work_for_target(&nt) < work_for_target(&g));
    }
}
