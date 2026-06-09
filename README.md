# Tao

A from-scratch **Proof-of-Work public blockchain** in Rust that is **compatible with the Solana ecosystem** (SPL Token, Anchor programs, Phantom, `@solana/web3.js`) by embedding the Solana SVM under a PoW consensus.

## Vision

- **Solana-compatible execution.** Full SVM smart contracts via the standalone [`solana-svm`](https://crates.io/crates/solana-svm) crate — existing programs and wallets work unchanged.
- **PoW consensus, staged.** Phase 1 is a linear PoW chain (longest-most-work rule); a `blockDAG`/GHOSTDAG upgrade is researched later.
- **Mining hardware evolution.** RandomX (CPU fair launch) → **matmul-PoUW** (GPU, AI-shaped Proof-of-Useful-Work with ZK verification, à la [Pearl](https://pearlresearch.ai)) → high-end GPUs.
- **AI as a first-class citizen.** GPU mining *is* matrix multiplication — the core AI operation — with a roadmap to bind the work to real model inference (genuine useful work).

See the full plan at [`docs/PLAN.md`](docs/PLAN.md).

## Workspace layout

| Crate | Role | Milestone |
|---|---|---|
| `tao-core` | Errors, config, logging, genesis, Solana-compatible primitives | M0 |
| `tao-node` | Full-node daemon | M0+ |
| `tao-consensus` | Block/header types, PoW, difficulty, chain selection | M1 |
| `tao-runtime` | Bank layer: AccountsDB + `solana-svm` execution | M2 |
| `tao-database` | RocksDB persistence | M1+ |
| `tao-p2p` | libp2p networking, IBD | M5 |
| `tao-mempool` | Tx pool + block templates | M2/M5 |
| `tao-rpc` | Solana-compatible JSON-RPC | M4 |
| `tao-wallet` | Keys + tx construction | M6 |
| `tao-cli` (`tao`) | CLI wallet + faucet | M6 |

## Build & run

```sh
cargo build
cargo run -p tao-node -- --version
cargo run -p tao-node -- init --data-dir .tao
cargo run -p tao-node -- run --config .tao/config.toml
```

## Status

- **M0 — scaffold ✓** Workspace, CLI, config/genesis bootstrap.
- **M1 — PoW + linear consensus ✓** Block/header types, `PowAlgorithm` trait
  (Blake3 now; RandomX → matmul-PoUW later), LWMA per-block difficulty,
  most-cumulative-work fork choice with reorgs, durable append-only block log
  with replay/resume, and a single-node CPU miner. 21 tests incl. a
  difficulty-convergence integration test.

Try it:

```sh
cargo run -p tao-node -- init --data-dir .tao
cargo run -p tao-node -- run --mine \
  --miner 11111111111111111111111111111111 --data-dir .tao --blocks 50
```

Next: **M2** — AccountsDB + `solana-svm` execution (the Solana-compatible
execution layer), with RocksDB state storage.

## License

ISC
