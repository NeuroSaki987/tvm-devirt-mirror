# Arena Live-Node Compaction Design

Date: 2026-09-26
Tracking issue: [jz0/tvm-devirt#6](https://github.com/jz0/tvm-devirt/issues/6)

## Objective

Prevent evaluator paths from stopping merely because their expression arena contains large amounts of unreachable history. When an arena crosses its configured node limit, recovery will rebuild the exact live expression DAG, remap every retained reference, and continue only if the live DAG fits within the limit.

The implementation is maintained and validated first in [NeuroSaki987/tvm-devirt-mirror](https://github.com/NeuroSaki987/tvm-devirt-mirror). An upstream pull request will be opened only after the complete validation matrix passes.

## Evidence and current behavior

`Arena::len()` currently counts every node created by a path. `Emulator::run` returns `Stop::Diverged` when this historical count exceeds `node_limit`.

Diagnostics from 33 divergence sites across six functions measured:

- 33,000,200 total nodes;
- 626,515 nodes reachable from complete semantic roots, or 1.90%;
- 32,373,685 unreachable nodes, or 98.10%;
- approximately 1,000,000 historical nodes and 19,000 live nodes per divergence.

The dominant dead nodes are temporary flag computations. Raising `node_limit` delays the failure while increasing clone and simplification costs; it does not address the cause.

## Scope

This change covers evaluator-owned arenas used while interpreting a path.

It includes:

- exact live-DAG rebuilding;
- remapping architectural state, current-block events, active pins, and caller-retained block-entry parameters;
- clearing ref-indexed derived caches;
- automatic compaction at the existing divergence boundary;
- unit, regression, cross-sample, stress, and memory validation.

It does not include:

- compaction of the completed CFG arena;
- SSA join repair or a higher block limit;
- new symbolic-load enumeration;
- guest indirect-tail-call modelling;
- changing expression simplification rules;
- changing the default node limit.

## Alternatives considered

### Exact rebuild into a fresh arena — selected

Traverse the reachable closure, rebuild it in dependency order, and return an old-to-new reference map. This removes dead history and keeps the existing dense `Ref` representation.

### Tombstones and free-list reuse — rejected

Keeping the old vector and reusing indices would make stale refs capable of silently naming unrelated nodes. It would also complicate interning and every direct index operation.

### Generational or per-block arenas — rejected for this change

This could reduce cloning further, but it changes ownership across forks, block grafting, and caches. It is a broader redesign and is unnecessary to prove the immediate recovery benefit.

## Safe compaction boundary

Automatic compaction occurs in `Emulator::run` before executing the next instruction, at the same point where the evaluator currently checks `node_limit`.

At that boundary there are no temporary instruction-local refs. All semantically retained expressions are owned by:

- `State`;
- current-block `Event` values;
- `Emulator::pins`;
- block-entry parameters retained by the CFG explorer outside `Emulator`.

The public `run` path has no external retained refs. CFG exploration will use a retaining run variant that accepts mutable external refs and remaps them whenever compaction occurs. This prevents a block parameter from becoming stale after the corresponding context slot is overwritten later in the block.

A stop returned by `step` is not exposed across a compaction boundary: compaction happens before `step`, and a returned stop is consumed before the next loop iteration.

## Complete root set

`State::symbolic_roots` already covers:

- GPR values;
- flag values;
- symbolic bytes stored at concrete addresses;
- both address and value of symbolic-address stores;
- both halves of tracked XMM registers.

Additional roots are:

- pin expressions;
- store/load event addresses and values;
- boxed-instruction memory addresses, definitions, and uses;
- call targets, argument values, and return opaques;
- caller-retained block-entry parameters.

Diagnostic reachability and production compaction will share the event-root enumeration helper so their definitions cannot drift.

Derived data is not a semantic root:

- `Arena::known_zero` and `known_one`;
- `Emulator::subst_memo`;
- `Emulator::pin_dep`.

These caches are cleared and rebuilt on demand.

## Exact Arena rebuilding

`Arena::compact` will:

1. Mark the transitive closure of all valid roots.
2. Produce an iterative post-order in which children precede parents.
3. Build a fresh node vector and intern map.
4. Recreate each node with remapped children and the original `Node { op, width }` identity.
5. Return an old-to-new map for every live node.
6. Preserve arena metadata that affects future identity:
   - `opaque_counter`;
   - `mem_gen`;
   - `sym_write_gen`;
   - `concrete_gen`;
   - `trunc_depth`.
7. Replace the old arena and leave known-bit caches empty.

Rebuilding uses direct node interning rather than `bin`, `select`, extension, or truncation constructors. Compaction must not run simplification again. Load generation numbers, counted opaque IDs, shared undef ID 0, and block parameter identities remain byte-for-byte equivalent at the node level.

Hash-consing is retained. Two old refs cannot represent structurally identical nodes in one arena; the rebuilt arena preserves that invariant and shared children remain shared.

## Ref remapping

Focused helpers will own remapping:

- `State::remap_refs` updates registers, flags, symbolic memory bytes, symbolic stores, and XMM values.
- `Event::append_roots` and `Event::remap_refs` cover every event variant.
- `Emulator::compact_arena` combines roots, calls `Arena::compact`, updates state/events/pins/external roots, clears caches, and returns before/after node counts.
- CFG exploration passes its retained entry-parameter refs through the retaining run method and writes the remapped refs back before grafting.

Every map lookup for a declared root is mandatory. A missing mapping is an invariant violation, not a reason to leave an old ref in place.

## Trigger and failure behavior

When `arena.len() > node_limit`:

1. Compact once from the complete root set.
2. If the compacted arena length is at or below `node_limit`, continue at the same instruction address.
3. If the compacted live arena still exceeds `node_limit`, return `Stop::Diverged` with the compacted live-node count.

Repeated compaction is permitted if a long block accumulates another limit-sized volume of dead history. The first version does not add a second heuristic or change the configured limit.

Recovery changes caused by continuing past a former divergence are expected. Changes in paths that never trigger compaction are not expected.

## Testing strategy

Development follows test-driven implementation. Each production primitive is introduced by a test that first fails because the capability is absent.

### Arena unit tests

- unreachable history is removed;
- every root remains valid and structurally equivalent;
- shared subexpressions remain shared;
- load width and generation are unchanged;
- counted opaque identities remain distinct and future opaque IDs do not collide;
- block parameters and shared undef nodes keep their identity;
- memory-generation and concrete-cell generation metadata still govern future loads correctly;
- deep DAG compaction is iterative and does not overflow the stack;
- duplicate and empty root sets behave correctly.

### Owner remapping tests

- every `State` holder is updated;
- every `Event` variant is updated;
- pins and caller-retained roots are updated;
- ref-indexed caches are cleared;
- no declared root is absent from the returned mapping.

### Trigger regression tests

With a deliberately small node limit:

- dead historical expressions cross the limit, compact, and execution continues;
- a genuinely live expression closure above the limit still returns `Diverged`;
- externally retained block parameters remain valid even after state overwrites their original slots.

### Repository verification

- `cargo test --release`;
- `cargo build --release`;
- `cargo clippy --release`, with existing baseline warnings distinguished from new warnings;
- `git diff --check`.

## Sample validation matrix

Final validation is intentionally broader than the initial PR #5 smoke tests.

### Cross-module baselines

At minimum:

| Module | Entry | Recorded baseline |
|---|---:|---:|
| ACE-Service64 | `0x1400059f0` | 8 blocks / 0 unresolved / 316 steps |
| ACE-CSI64-s2 | `0x18006cee0` | 25 / 0 / 175,389 |
| ACE-Tray | `0x140013cc0` | 64 / 18 / 1,342 |
| SGuardAgent64 | `0x18000c0e0` | 57 / 0 / 228,460 |
| ACE-DFS64-s2 | `0x180093320` | 594 / 101 / 2,328,811 |
| ACE-DFS64-s2 | `0x180017360` | 30 / 1 / 693,938 |

The first four cover fully resolved and genuinely unresolved paths that should remain stable. The DFS64 entries cover large CFG and known divergence behavior.

### DFS64 corpus

Before an upstream PR:

- run all 157 previously identified partially recovered functions through the diagnostic command in isolated processes;
- compare final blocks, unresolved classifications, steps, timeout status, and CFG validation with the saved baseline;
- run all known `Diverged` functions with enough time to observe whether compaction removes or moves the failure;
- retain complete JSON results and an aggregate comparison.

Where resources permit, rerun the full 425-function recovery inventory to detect changes outside the partial-function corpus.

### Acceptance rules

- No crash, panic, invalid ref, OOM, or malformed JSON.
- No new CFG validation issue.
- No unexplained change in any path that did not compact.
- A changed divergent path must be traceable to a recorded compaction and must preserve executable-target and SSA invariants.
- Pre-existing validation issues may not increase or change category without a separately explained root cause.
- Peak working set and elapsed time are recorded for the large DFS64 case; compaction must not worsen the sustained memory footprint.
- Any unexplained mismatch blocks both mirror merge and upstream PR.

## Delivery workflow

1. Track the design and discussion in upstream Issue #6.
2. Implement on a dedicated branch in `tvm-devirt-mirror`.
3. Keep the mirror branch pushed as validation progresses.
4. Publish corpus comparison artifacts or a concise reproducible summary.
5. Request code review after the implementation and again after corpus validation.
6. Merge into the mirror only after all acceptance rules pass.
7. Open a focused upstream PR linked to Issue #6 after mirror validation is complete.
