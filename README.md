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

- **M2 — accounts + SVM execution ✓** RocksDB `AccountsDb` with deterministic
  `state_root`; `BlockhashQueue`; genesis allocation loader; and a `Bank` that
  **executes real Solana transactions through the embedded Agave SVM**
  (`solana-svm` 4.0), with coinbase (block reward + recycled fees) and
  block-level execution. The miner executes each block, stamps `state_root`
  into the header, and on restart the block log is **replayed and re-executed,
  verifying every block's state_root**. Proven: a System transfer runs
  unchanged (rent-exemption enforced); two independent banks reach an identical
  state root; coinbase credits the miner per block. 31 tests.

Try it (watch coinbase + state_root, then restart to replay/verify):

```sh
cargo run -p tao-node -- run --mine \
  --miner So11111111111111111111111111111111111111112 --data-dir .tao --blocks 5
```

Next: **M3** (deploy SPL Token + run an Anchor program) and **M4**
(Solana-compatible JSON-RPC so Phantom / web3.js can submit transactions).

## License

ISC
