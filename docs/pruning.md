# blockDAG pruning (M8)

How a node bounds growth by discarding finalized history. Two layers:

1. **Transaction-body pruning (done, `prune_finalized_transactions`)** — drop the
   transaction bodies of blocks in the checkpoint prefix. Their effects are in the
   checkpoint snapshot, so they are redundant for state. Consensus-safe: touches
   no ordering / difficulty / GHOSTDAG computation (headers + engine retained).

   **Durability note:** this operation is intentionally lightweight and **not
   compacted to `dag.log` by itself**. On restart, full block logs are replayed
   and transaction bodies reappear, so this is mainly a live-memory optimization.
   Use `prune` after checkpointed finalize pruning for durable, restart-persistent
   history shrink on both headers and state.

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

## Trustless pruning proof (NiPoPoW) — done

Bootstrap is now trustless: a syncing node verifies, by PoW alone, that the
pruning point descends from genesis on a real chain, before adopting its snapshot.

1. **Block level (`pow_level`).** How many extra leading zero bits a PoW hash
   cleared beyond its target — level k ⇒ a 2^k-rarer solution. High-level blocks
   sample accumulated work, making the proof succinct.
2. **Interlinks (`DagBlockHeader.interlink`).** `interlink[k]` = the most recent
   selected ancestor of level ≥ k. Computed at build time from the selected
   parent (`interlink_for_parent`), PoW-committed, and **validated in `accept`**
   so it can't be forged.
3. **Proof construction (`build_proof_for`).** Walk the highest-level interlink
   back-pointer at each step from P until genesis (empty interlink): each jump is
   the longest available, so the proof is logarithmic-ish in the work yet always
   interlink-connected (consecutive headers are linked by the followed pointer)
   and anchored at genesis. A longer chain yields a longer walk (more high-level
   blocks ⇒ more certified work), which is what the most-work comparison needs.
   Built before pruning (while ancestors exist) and retained. (A multi-level
   "Prove" construction sampling *all* level-μ blocks was tried but left
   connectivity gaps on long chains; the connected single walk is used instead.)
4. **Verification (`verify_proof`).** A joiner checks the proof is anchored at its
   own genesis, ends at the claimed origin, every non-genesis header has valid
   PoW, and consecutive headers are interlink/parent-connected; it returns the
   accumulated work. `import_snapshot` runs this before adopting a snapshot.

Tested: `snapshot_sync_bootstraps_a_fresh_node` (verified bootstrap),
`import_rejects_invalid_proof`, `rejects_forged_interlink`.

## Audit Notes (items 11/13)

### 11) `prune_finalized_transactions` durability contract

This API is explicitly **in-memory only**: it drops transaction bodies from
`block_txs` for finalized-prefix blocks to bound hot memory, but it does **not**
compact `dag.log`. The checkpoint snapshot remains the full replay source, so
after restart the full log replay restores those dropped bodies. In practice:

- immediate effect: memory drops, state rebuild stays consistent via
  `virtual_state` + checkpoint;
- durable effect: none, until `prune` runs and rewrites the log (`Snapshot + suffix`)
  to drop headers/blocks permanently.

Implemented in `DagChain::prune_finalized_transactions` and documented in its
method docstring.

### 13) Serialization production-path robustness

The production panic risk on header/block-serialization failure is reduced in
the Dag consensus path:

- `BlockHeader::serialize` and `DagBlockHeader::serialize` in
  `crates/tao-consensus/src/block.rs` now use
  `unwrap_or_else(|e| { warn!; Vec::new() })` instead of `expect`.
- Dag path persistence/deserialization in `crates/tao-dagvm/src/dag_chain.rs`
  follows the same pattern where practical.

So the specific review risk (hard panic on serialization failure in this path) is
addressed, while still leaving intentionally non-critical fallbacks to surface via
logs.

### Refinements

- **Most-work selection across competing peers — done.** `import_snapshot`
  verifies each candidate proof and adopts only the highest-work one (the NiPoPoW
  "maxchain" rule): a fresh node adopts, a bootstrapped node switches only to a
  strictly more-work proof, and a node that built/pruned its own chain never
  adopts. `verify_proof` returns the certified work; proofs anchor at the node's
  original genesis. Tested by `most_work_proof_wins`.
- **Multi-prune proof chaining — remaining.** A genesis-anchored proof is rebuilt
  at each prune; once a node has pruned twice, the older ancestors are gone, so
  the proof anchors at genesis only while built from headers that still reach it.
  Chaining successive proofs across repeated prunes is the last refinement.
