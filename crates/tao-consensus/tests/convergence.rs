//! Integration test: LWMA difficulty converges to the target block time under
//! a constant-hashrate miner.
//!
//! We model a miner doing `HASHRATE` hashes/second. The expected number of
//! hashes to solve a block equals its chain work, so the solve time for a
//! target is `work(target) / HASHRATE` seconds. We feed those timestamps back
//! into the difficulty algorithm and check that, after warmup, the average
//! block interval settles near the configured target — i.e. difficulty tracks
//! hashrate. This is the deterministic version of "watch the live miner
//! converge", with no wall-clock flakiness.

use tao_consensus::{
    block::HEADER_VERSION, difficulty::next_target, work_for_target, BlockHeader, DifficultyParams,
};
use tao_core::{Hash, Pubkey};

const HASHRATE: u128 = 1_000_000; // 1 MH/s
const TARGET_BLOCK_TIME: u64 = 10; // seconds
const WINDOW: u64 = 90;

fn work_u128(target: &[u8; 32]) -> u128 {
    let w = work_for_target(target);
    if w <= primitive_types::U256::from(u128::MAX) {
        w.as_u128()
    } else {
        u128::MAX
    }
}

/// Modeled solve time (seconds) for a given target at constant hashrate.
fn modeled_solvetime(target: &[u8; 32]) -> i64 {
    let expected_hashes = work_u128(target);
    ((expected_hashes / HASHRATE) as i64).max(1)
}

fn header(height: u64, timestamp: i64, target: [u8; 32]) -> BlockHeader {
    BlockHeader {
        version: HEADER_VERSION,
        prev_hash: Hash::default(),
        height,
        timestamp,
        tx_merkle_root: Hash::default(),
        state_root: Hash::default(),
        target,
        nonce: 0,
        miner: Pubkey::default(),
    }
}

#[test]
fn difficulty_converges_to_target_block_time() {
    let params = DifficultyParams::new(TARGET_BLOCK_TIME, WINDOW);

    // Genesis: an easy 8-zero-bit target, far below the equilibrium difficulty.
    let mut genesis_target = [0xffu8; 32];
    genesis_target[0] = 0x00;

    let mut chain: Vec<BlockHeader> = vec![header(0, 1_000_000, genesis_target)];

    // Mine 700 blocks driven by the difficulty algorithm + the hashrate model.
    for h in 1..=700u64 {
        let target = next_target(&chain, &params, &genesis_target);
        let solvetime = modeled_solvetime(&target);
        let timestamp = chain.last().unwrap().timestamp + solvetime;
        chain.push(header(h, timestamp, target));
    }

    // Average block interval over the last 200 blocks (well past warmup).
    let tail = &chain[chain.len() - 200..];
    let span = tail.last().unwrap().timestamp - tail.first().unwrap().timestamp;
    let avg_interval = span as f64 / (tail.len() as f64 - 1.0);

    // Should sit within ±25% of the 10s target.
    assert!(
        avg_interval > 7.5 && avg_interval < 12.5,
        "avg interval {avg_interval:.2}s did not converge near {TARGET_BLOCK_TIME}s"
    );

    // And difficulty must have risen far above genesis (it tracked hashrate up).
    let final_work = work_u128(&chain.last().unwrap().target);
    let genesis_work = work_u128(&genesis_target);
    assert!(
        final_work > genesis_work * 1000,
        "difficulty barely moved: {genesis_work} -> {final_work}"
    );
}
