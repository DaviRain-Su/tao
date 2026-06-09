# Utility Gate — binding matmul-PoUW to real model inference

This is the design that closes the gap Pearl leaves open: making the mining work
**provably useful**, not merely "AI-shaped."

## The problem (Pearl's open flaw)

Pearl's PoW proves a matrix multiplication was computed *correctly* (via a STARK
proof), but **not that the matrices mean anything**. A miner can multiply random
matrices and still win. Independent analysis (arXiv 2606.04819) found Pearl's
live network does ≈zero useful AI for exactly this reason — its `verify_plain_proof`
checks computational correctness but never *utility*, and a random-matrix check
is trivially spoofed.

So the missing piece is a **binding**: the puzzle's inputs must be a real model's
weights and a real inference request, in a way anyone can verify cheaply.

## The design

Make the puzzle matrices **not free**:

1. **Model registry (on-chain).** A model is registered with a **Merkle
   commitment over its weight tiles** (each tile an `n×n` block). The model id
   commits to that root, so the weights can't be swapped after the fact.
   - `register(name, n, tiles) -> model_id`; the weight root is the Merkle root
     of `H(tile)` leaves (padded to a power of two).

2. **Work item (the request).** `WorkItem { model_id, tile_index, input }` — a
   user wants model `M`'s tile `T` applied to a specific `input` (and pays for
   it). `commitment() = H(model_id ‖ tile_index ‖ H(input))` binds the whole
   task into one value.

3. **Bound puzzle.** A valid solution must use
   - `A` = the model's committed weight tile, **proven by a Merkle proof against
     the model's weight root** at `tile_index`, and
   - `B` = the requested `input`.

   The **nonce only seeds the low-rank noise** `E,F` (for PoW grinding) via
   `noise_seed = H(work_commitment ‖ nonce)`. The underlying `A·B` is therefore
   **fixed by the task** — every grind attempt computes the same real inference,
   just with different noise. The PoW value is the transcript hash of the noised
   product `(A+E)(B+F)` checked against the difficulty target.

4. **Useful output.** `A·B` (the real layer result) is recovered cheaply from the
   noised product via the low-rank correction (here: computed directly) and
   returned to the requester. Mining a block and producing a real inference
   result are the **same act** — genuine Proof-of-Useful-Work.

## Why forgery fails (the anti-fake property)

To win, a miner must present a `weight_tile` plus a Merkle proof that it sits
under the registered model's `weight_root` at `tile_index`. Random/fabricated
weights have no valid proof under the real root, so the gate rejects them
(`GateError::ForgedWeights`). The input is pinned by the work-item commitment
(`WorkMismatch` / `InputMismatch` otherwise). Hence the matmul is provably a real
model computation — the property Pearl lacks.

The `tao-pouw::utility_gate` tests demonstrate exactly this: a genuine
computation is accepted and yields the real `A·B`; the random-matrix attack is
rejected by the Merkle check.

## Verification cost & the ZK upgrade

In this prototype the verifier **recomputes** the matmul to check the PoW — fine
for correctness, too expensive for a real chain. Production keeps this exact
binding protocol but replaces "recompute to verify" with a **Plonky2 STARK proof**
that attests:
1. the GEMM `(A+E)(B+F)` was computed correctly and meets the target, and
2. `A` is the committed tile (the Merkle path is checked *inside the circuit*),
3. `B` is the committed input.

Verifiers then check a ~60 KB proof in milliseconds. The GEMM itself runs on a
GPU (Pearl's `pearl-gemm` kernels). None of that changes the design above — it
only changes *how cheaply* the binding is verified.

## Open questions (honest)

- **Input availability / DA.** The requester's input and the produced output
  need a data-availability path so the result is retrievable; here they're passed
  alongside the solution.
- **Demand & anti-griefing.** Real usefulness needs a real stream of paid
  requests; without demand the chain should fall back to (and be honest about
  being) plain "AI-shaped" PoW rather than claiming utility.
- **Quantization & determinism.** Real models use floats; the PoW needs
  determinism, so production must pin an integer/fixed-point quantization of the
  model (committed in the registry) — the registry already commits to exact
  integer tiles, which is the right hook.
- **Tile granularity.** One tile per puzzle is coarse; batching tiles / whole
  layers per block and scheduling many requests is future work.

See `crates/tao-pouw/src/utility_gate.rs` for the implementation and tests.
