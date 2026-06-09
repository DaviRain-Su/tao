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

- **M3 — SPL Token + program deployment ✓ (core)** The Bank registers a real
  syscall loader (`agave-syscalls`) and the BPF loader builtins, and can
  `deploy_program` an sBPF `.so`. Proven: the **real mainnet SPL Token program**
  (`programs/spl_token.so`) is deployed and executed end-to-end — create +
  initialize a mint, initialize a token account, `MintTo`, and the token
  balance reads back correctly. Anchor programs run via the same path. 32 tests.

- **M4 — Solana-compatible JSON-RPC ✓** An axum JSON-RPC server with the core
  method set (`getHealth`, `getVersion`, `getSlot`, `getLatestBlockhash`,
  `getBalance`, `getAccountInfo`, `getMinimumBalanceForRentExemption`,
  `getFeeForMessage`, `sendTransaction`, `getSignatureStatuses`, ...) returning
  Solana's `{context, value}` shapes. A mempool feeds the miner, which records
  signature statuses. **Verified end-to-end with the real `@solana/web3.js`
  SDK**: it connected, submitted a `SystemProgram.transfer`, the tx was mined +
  executed via the SVM + confirmed, and balances matched (amount + 5000 fee).
  Run with `--rpc`:

```sh
cargo run -p tao-node -- run --mine --rpc \
  --miner So11111111111111111111111111111111111111112 --data-dir .tao
```

- **M5 — P2P networking ✓** A minimal TCP gossip layer (`tao-p2p`): nodes
  listen + dial bootstrap peers and relay `NewBlock` / `NewTx`. The node loop
  applies inbound peer blocks (validate PoW/difficulty → execute → verify
  state_root) and follower nodes track the miner; transactions submitted to any
  node's RPC gossip to the miner. **Verified with a 3-node testnet** (1 miner +
  2 followers): all nodes stayed at the same height, and a transfer submitted to
  a *follower's* web3.js RPC propagated to the miner, was mined, and the
  recipient balance was identical on all three nodes. (Star topology + single
  miner sidesteps reorgs; libp2p + IBD + multi-miner are future work.)

```sh
# miner
tao-node run --mine --miner <PUBKEY> --data-dir n1 --listen 127.0.0.1:9001 --rpc --rpc-port 8899
# follower
tao-node run --data-dir n2 --listen 127.0.0.1:9002 --peers 127.0.0.1:9001 --rpc --rpc-port 8900
```

- **M6 — CLI wallet + faucet + devnet ✓** The `tao` CLI (`keygen`, `address`,
  `balance`, `airdrop`, `transfer`) builds + signs transactions locally and
  talks to the node's RPC. A node-side faucet (`requestAirdrop`) signs a real
  transfer from a genesis-funded faucet account (replayable). `scripts/devnet.sh`
  launches a one-command devnet (mining + RPC + faucet). Verified: keygen →
  airdrop 2 TAO → balance → transfer 0.5 TAO → balances reflect amount + fee.

```sh
./scripts/devnet.sh .tao-devnet        # terminal 1
tao keygen -o wallet.json              # terminal 2
tao airdrop $(tao address -k wallet.json) 2000000000
tao balance $(tao address -k wallet.json)
```

- **M7 — matmul-PoUW (AI-shaped PoW) — prototype ✓** `tao-pouw::MatmulPow`
  implements the `PowAlgorithm` trait with a NoisyGEMM-style puzzle: low-rank
  noise is added to seed-derived matrices, the noised product is computed
  (`O(n³)` integer matmul — the work), and a transcript hash is checked against
  the target. `HeightSwitchPow` activates a new algorithm at a fork height. A
  test mines a real chain across a **Blake3 → matmul-PoUW switch**, the
  RandomX→GPU evolution mechanism. *Prototype = CPU integer matmul, verified by
  recomputation.* Production (future) adds GPU CUDA kernels, Plonky2 STARK
  proofs (cheap verify), and a utility gate binding the work to a real model.
  - **Utility gate** (`tao-pouw::utility_gate`, see [`docs/utility-gate.md`](docs/utility-gate.md)):
    the design that closes Pearl's open flaw (its network does ≈zero real AI).
    A model registry commits to weights via a Merkle root; a work item binds
    (model, tile, input); a valid solution must use the **committed** weight tile
    (proven by a Merkle proof) and the requested input — the nonce only seeds the
    noise, so `A·B` is the real inference result. Random/forged matrices are
    rejected by the Merkle check. Verified by tests (accepts real work + returns
    `A·B`; rejects the random-matrix attack).

- **M8 — blockDAG / GHOSTDAG — prototype ✓** `tao-dag` implements the core of
  Kaspa's consensus: blocks reference multiple parents, and GHOSTDAG classifies
  every block **blue** (well-connected) or **red** (mined ignoring too much of
  the DAG) via the k-cluster rule, then produces a deterministic **total order**.
  Tests show a linear chain stays all-blue, `k=0` degenerates to longest-chain, a
  diamond merges the parallel block as blue, and a block that ignores the DAG is
  marked red. *Prototype = explicit `past` sets, `u64` ids.* Production needs an
  efficient reachability index, pruning, and feeding the linear order into the
  SVM (the "Case B" linearization).

**M0–M6 complete; M7 (matmul-PoUW + utility gate) and M8 (GHOSTDAG) prototyped.**
Remaining (per `docs/PLAN.md`): production-hardening these research tracks (GPU
kernels, Plonky2 ZK, reachability/pruning, SVM-on-DAG), plus libp2p/IBD/
multi-miner reorgs and a real RandomX CPU phase.

## License

ISC
