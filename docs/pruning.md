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

## Persistence & sync (done)

- **Durable log compaction.** On prune, `dag.log` is atomically rewritten as
  `[Snapshot(origin header + base accounts), Block(kept suffix)…]`
  (`BlockLog::replace_all`), so a restart replays the snapshot + suffix and
  reproduces the pruned chain — it does not un-prune. `LogRecord` tags each
  record; `apply` remaps pruned-ancestor parent references to the origin.
- **Serving sync from the pruning point.** A pruned node ships `SyncSnapshot`
  { origin header, base account set, kept suffix } via `NetMsg::GetSnapshot` /
  `Snapshot`. A fresh/behind node adopts it (`import_snapshot`): re-anchor +
  apply the trusted suffix + write a compacted log. `dag-run` requests a snapshot
  when block backfill stalls on pruned ancestors.

## Remaining gap to full Kaspa parity: the trustless pruning proof

The shipped snapshot is currently *trusted* (adopted like a checkpoint). Kaspa
makes it trustless with a PoW **pruning-point proof** (a NiPoPoW): a succinct set
of headers that certifies the pruning point lies on the most-work chain, without
the pruned history. The foundation — the **block level** (`pow_level`, how many
extra zero bits a PoW hash cleared, so a 2^level-rarer event) — is implemented.
Completing the proof is a focused, security-critical milestone:

1. **Interlinks (header change).** Each header commits to an `interlink` vector:
   `interlink[k]` = the most recent selected ancestor of level ≥ k. Computed at
   mine time from the selected parent: `new = sp.interlink` with `interlink[k] =
   sp` for all `k ≤ level(sp)`. PoW commits to it (so it can't be forged), and
   `accept` must re-derive and validate it (else the proof is unsound).
2. **Proof construction (retained before pruning).** Walk interlinks at the
   highest level μ that still has ≥ m blocks from genesis to P, collecting those
   headers — `O(m·log(work))` headers, small enough to retain after pruning.
3. **Verification.** A syncing node checks each proof header's PoW and level, that
   they form an interlink-connected chain genesis→P, and accepts the proof with
   the most accumulated work (NiPoPoW "most-work" rule) — then adopts that P's
   snapshot. This replaces the trusted-suffix application with a verified one.

This touches the header, the mining path, and `accept` validation, so it is left
as a dedicated step rather than folded into the pruning work above.
