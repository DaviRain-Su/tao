# blockDAG pruning (M8)

How a node bounds growth by discarding finalized history. Two layers:

1. **Transaction-body pruning (done, `prune_finalized_transactions`)** — drop the
   transaction bodies of blocks in the checkpoint prefix. Their effects are in the
   checkpoint snapshot, so they are redundant for state. Consensus-safe: touches
   no ordering / difficulty / GHOSTDAG computation (headers + engine retained).

2. **Re-anchor pruning (this document, `prune`)** — discard the *headers and
   GHOSTDAG/reachability data* of finalized blocks too, by re-rooting the DAG at a
   finalized **pruning point** `P`. `P` becomes the new origin; its past is gone;
   its account state becomes the new "genesis" state.

## The pruning point

`P` is chosen on the **selected chain** (genesis→tip), deep behind the tip. This
matters for two reasons:

- **Clean prefix.** For a selected-chain block `C`, every block ordered before
  `C` in the GHOSTDAG total order is in `past(C)`. So cutting the total order at a
  selected-chain block gives `prefix = past(P) ∪ {P}` with no "anticone leakage":
  every pruned block is an ancestor of `P`. A kept block that referenced a pruned
  parent simply remaps that parent to `P` (the pruned block's effects live in the
  snapshot, and `P` abstracts "all history through `P`").
- **DAA-window consistency (fork safety).** Difficulty for a block is an LWMA over
  its `lwma_window+1` selected ancestors. A pruned and a non-pruned node MUST
  compute the same target for any block they both validate, or they reject each
  other's blocks and fork. New blocks arrive at the tip; as long as `P` is at
  least `lwma_window` (plus a finality margin) behind the tip, every new block has
  a full window of *retained* (post-`P`) selected ancestors, so both nodes compute
  the identical target. Old blocks within a window of `P` are already finalized and
  are never re-validated by a pruned node.

So the prune trigger is: `depth(tip) - depth(P) ≥ lwma_window + finality_depth`,
with `P` the deepest selected-chain block satisfying it.

## Mechanism

1. Pick `P` (selected-chain, deep enough). Let `prefix = order[..=idx(P)]`.
2. Compute the **base state** = account set after executing `prefix` (from the
   previous base/genesis, replaying only up to `P` — bounded work). This full
   `AccountSharedData` set (not just lamport allocations: it includes deployed
   programs, token accounts, data) becomes the post-prune genesis state.
3. Rebuild the GHOSTDAG engine with `P` as genesis and re-add the kept suffix
   `order[idx(P)+1..]` in topological order. Each kept block's parents that fall
   in the pruned past are remapped to `P`; parents in the kept set are preserved.
   Blue score/work reset relative to `P=0`; relative ordering of kept blocks is
   preserved (a uniform shift), so the total order suffix is unchanged.
4. Drop headers / `block_txs` / block-set entries for pruned blocks; keep the
   suffix. `base_accounts` replaces the genesis allocations for state rebuilds.

## Invariants (tested)

- **INV1 (state):** `virtual_state` after a prune == before it (the base captures
  the pruned prefix exactly).
- **INV2 (order):** the post-prune total order == the pre-prune total order from
  `P` onward (re-anchoring preserves the suffix linearization).
- **INV3 (difficulty):** with a full retained window, `next_target` / the
  enforced `expected_target` after a prune equals a non-pruned chain's — no fork.

## Deliberately deferred

- **Durable log compaction.** `dag.log` still holds full history; a restart
  replays it and rebuilds un-pruned. Compacting it to start from a snapshot record
  is mechanical follow-on.
- **Serving sync from the pruning point.** A pruned node can't serve from-genesis
  joiners; it would ship the base snapshot + post-`P` blocks (Kaspa pruning-proof
  analogue). Not yet implemented.
