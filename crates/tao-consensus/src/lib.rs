//! `tao-consensus` — the linear PoW consensus core.
//!
//! - [`block`]: block/header types and Merkle roots.
//! - [`target`]: 256-bit PoW target and chain-work arithmetic.
//! - [`pow`]: the [`pow::PowAlgorithm`] abstraction (Blake3 now; RandomX →
//!   matmul-PoUW later).
//! - [`difficulty`]: per-block LWMA difficulty adjustment.
//! - [`chain`]: in-memory chain state with most-cumulative-work fork choice.
//!
//! Scaffold for milestone **M1**.

pub mod block;
pub mod chain;
pub mod difficulty;
pub mod genesis;
pub mod mine;
pub mod pow;
pub mod target;

pub use block::{
    tx_merkle_root, Block, BlockHeader, BlockId, DagBlock, DagBlockHeader, HEADER_VERSION,
};
pub use chain::{BlockStatus, ChainError, ChainState};
pub use difficulty::{next_target, DifficultyParams};
pub use genesis::genesis_header;
pub use mine::{grind, GrindResult};
pub use pow::{Blake3Pow, HeightSwitchPow, PowAlgorithm};
pub use target::{meets_target, work_for_target, Target};
