# Arena Live-Node Compaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reclaim unreachable evaluator expression nodes at the existing node-limit boundary, remap every retained reference, and continue only when the exact live DAG fits the configured limit.

**Architecture:** `Arena` performs an iterative, identity-preserving rebuild and returns an old-to-new reference map. `State`, `Event`, and `Emulator` own remapping of their references; CFG exploration explicitly lends block-entry parameters to the emulator while it runs. The public run path delegates to the retaining path with no external roots.

**Tech Stack:** Rust 2024, standard-library collections, the existing `cargo test` harness, PowerShell for isolated sample orchestration.

**Spec:** `docs/superpowers/specs/2026-09-26-arena-live-node-compaction-design.md`

## Global Constraints

- Keep the default node limit and expression simplification rules unchanged.
- Compact only evaluator-owned arenas at the existing pre-instruction limit check; do not compact the completed CFG arena.
- Rebuild raw `Node { op, width }` identities without calling simplifying expression constructors.
- Preserve `opaque_counter`, `mem_gen`, `sym_write_gen`, `concrete_gen`, and `trunc_depth`; clear all ref-indexed derived caches.
- Treat every missing remap for a declared root as an invariant violation.
- Maintain and validate the change in `NeuroSaki987/tvm-devirt-mirror` before opening an upstream pull request linked to `jz0/tvm-devirt#6`.
- Any unexplained sample mismatch, crash, panic, invalid reference, OOM, malformed JSON, or new CFG validation issue blocks merge and upstream submission.

## Review Focus

- Empty or duplicate root lists must compact deterministically without retaining dead nodes; Task 1 tests both.
- A malformed out-of-range root must fail before replacing the arena; Task 1 tests that the original arena remains intact.
- Deep unary/load chains must compact iteratively without stack overflow; Task 1 exercises a chain substantially deeper than the call stack.
- Every optional and collection-valued `Event` reference must be rooted and remapped; Task 2 constructs all event variants, including absent `Call`/`Boxed` optionals.
- A block-entry parameter whose original state slot is overwritten before compaction must still be remapped and graftable; Task 4 adds an explorer-level regression.

---

### Task 1: Exact Arena compaction

**Files:**
- Modify: `src/ir/expr.rs`

**Interfaces:**
- Produces: `pub fn compact(&mut self, roots: &[Ref]) -> HashMap<Ref, Ref>`.
- Guarantees: every returned value is valid in the rebuilt arena; every input root and its transitive operands has a mapping; arena identity metadata is preserved and known-bit caches are empty.

- [ ] **Step 1: Add failing Arena behavior tests**

Add focused tests named `compact_removes_unreachable_nodes_and_preserves_structure`, `compact_preserves_shared_children_and_exact_node_identity`, `compact_preserves_identity_generations_and_future_construction`, `compact_handles_empty_and_duplicate_roots`, `compact_rejects_invalid_roots_without_mutating_arena`, and `compact_handles_a_deep_dag_iteratively`. Assert structural hashes, exact `Op`/`Width` values (including load generation, counted opaque, undef, and parameter identities), shared child refs, before/after lengths, future opaque IDs and load generations, and unchanged arena state after invalid input.

- [ ] **Step 2: Run the new tests and verify RED**

Run: `cargo test --release compact_ -- --nocapture`

Expected: compilation fails because `Arena::compact` does not exist.

- [ ] **Step 3: Implement iterative closure ordering and raw rebuild**

Implement `Arena::compact(&mut self, roots: &[Ref]) -> HashMap<Ref, Ref>` in `src/ir/expr.rs`. Validate all roots before mutation, use an explicit `(Ref, visited_children)` stack to obtain child-before-parent order, remap operand refs by matching every `Op` variant, insert the resulting raw `Node` directly into fresh `nodes` and `intern`, preserve the five identity metadata fields, and replace the arena only after the rebuild succeeds.

- [ ] **Step 4: Run focused and full tests and verify GREEN**

Run: `cargo test --release compact_ -- --nocapture`

Expected: all six new compaction tests pass.

Run: `cargo test --release`

Expected: the complete suite passes with zero failures.

- [ ] **Step 5: Commit the independently usable primitive**

```powershell
git add src/ir/expr.rs
git commit -m "feat: compact expression arenas to live nodes"
git push
```

### Task 2: State and Event ownership helpers

**Files:**
- Modify: `src/vm/state.rs`
- Modify: `src/ir/lift.rs`

**Interfaces:**
- Consumes: `Arena::compact(&mut self, roots: &[Ref]) -> HashMap<Ref, Ref>` from Task 1.
- Produces: `pub(crate) fn State::remap_refs(&mut self, remap: &HashMap<Ref, Ref>)`.
- Produces: `pub(crate) fn Event::append_roots(&self, roots: &mut Vec<Ref>)` and `pub(crate) fn Event::remap_refs(&mut self, remap: &HashMap<Ref, Ref>)`.

- [ ] **Step 1: Add failing State remapping tests**

Extend the state root test module with `remap_refs_updates_every_machine_state_holder` and `remap_refs_panics_on_a_missing_declared_root`. Populate registers, flags, symbolic concrete-address bytes, both sides of symbolic stores, and XMM halves with refs whose indices change after compaction; assert every holder contains its mapped ref.

- [ ] **Step 2: Run State tests and verify RED**

Run: `cargo test --release remap_refs_ -- --nocapture`

Expected: compilation fails because `State::remap_refs` does not exist.

- [ ] **Step 3: Implement `State::remap_refs`**

Update every holder enumerated by `State::symbolic_roots`, using mandatory map indexing or `expect` so no stale ref can survive.

- [ ] **Step 4: Add failing Event root/remapping tests**

In `src/ir/lift.rs`, add `event_roots_and_remapping_cover_every_variant` and `event_remapping_handles_absent_optionals`. Construct Store, Load, Boxed, and Call events with unique refs; assert root enumeration and every remapped field, while `None` fields remain `None`.

- [ ] **Step 5: Run Event tests and verify RED**

Run: `cargo test --release event_roots_and_remapping -- --nocapture`

Expected: compilation fails because the Event helper methods do not exist.

- [ ] **Step 6: Implement Event ownership helpers**

Implement the two methods on `Event`, covering Store/Load address and value, Boxed memory/defs/uses, and Call target/args/ret. Keep non-expression fields unchanged.

- [ ] **Step 7: Run owner tests and the complete suite**

Run: `cargo test --release remap_refs_ event_roots_and_remapping -- --nocapture`

If Cargo accepts only one filter, run the two filters separately.

Run: `cargo test --release`

Expected: all tests pass with zero failures.

- [ ] **Step 8: Commit owner helpers**

```powershell
git add src/vm/state.rs src/ir/lift.rs
git commit -m "feat: remap evaluator expression owners"
git push
```

### Task 3: Emulator compaction and node-limit recovery

**Files:**
- Modify: `src/ir/lift.rs`

**Interfaces:**
- Consumes: Task 1 and Task 2 interfaces.
- Produces: `fn compact_arena(&mut self, retained: &mut [Ref]) -> (usize, usize)`.
- Produces: `pub(crate) fn run_retaining(&mut self, start: u64, budget: usize, retained: &mut [Ref]) -> Stop`.
- Preserves: `pub fn run(&mut self, start: u64, budget: usize) -> Stop`, delegating with an empty retained slice.

- [ ] **Step 1: Add failing direct compaction tests**

Add tests `emulator_compaction_remaps_state_events_pins_and_external_roots`, `emulator_compaction_clears_ref_indexed_caches`, and `emulator_compaction_reports_before_and_after_counts`. Build a minimal in-memory `PeFile` from its public fields with one executable section, make all owner categories point to live nodes after dead history, seed `subst_memo`/`pin_dep`, compact, and assert valid mapped refs, empty caches, synchronized `memo_pin_count`, and exact counts.

- [ ] **Step 2: Run direct compaction tests and verify RED**

Run: `cargo test --release emulator_compaction_ -- --nocapture`

Expected: compilation fails because `Emulator::compact_arena` does not exist.

- [ ] **Step 3: Implement the complete root collection and remapping transaction**

Implement `compact_arena`: combine `State::symbolic_roots`, `pins`, every event via `append_roots`, and the caller slice; call `Arena::compact`; remap State, events, pins, and retained refs; clear `subst_memo` and `pin_dep`; set `memo_pin_count` to the current pin count; return historical and live counts.

- [ ] **Step 4: Add failing run-trigger regressions**

Add `run_compacts_dead_history_and_continues`, `run_diverges_when_the_live_dag_still_exceeds_the_limit`, and `run_retaining_preserves_external_roots_across_compaction`. Use a deliberately small `node_limit` and a minimal in-memory PE whose executable section contains a `ret`. Assert the dead-history case reaches the same non-Diverged stop as its uncompacted control, the live-over-limit case returns `Stop::Diverged` with the compacted count, and an external ref remains renderable/structurally equivalent after execution.

- [ ] **Step 5: Run trigger regressions and verify RED**

Run: `cargo test --release run_compacts_ -- --nocapture`

Run: `cargo test --release run_diverges_ -- --nocapture`

Run: `cargo test --release run_retaining_ -- --nocapture`

Expected: at least the dead-history and retaining cases fail under the existing immediate-divergence behavior.

- [ ] **Step 6: Implement retaining run and limit-trigger behavior**

Move the loop to `run_retaining`. When `arena.len() > node_limit`, compact exactly once at the current IP, continue if the live length is within the limit, and otherwise return `Stop::Diverged { site: ip, nodes: live_len }`. Make `run` call it with `&mut []`.

- [ ] **Step 7: Run focused and complete tests**

Run the three trigger filters, then `cargo test --release`.

Expected: all tests pass with zero failures; paths below the limit preserve prior behavior.

- [ ] **Step 8: Commit runtime recovery**

```powershell
git add src/ir/lift.rs
git commit -m "feat: recover evaluator node limits by compaction"
git push
```

### Task 4: Preserve CFG explorer entry parameters

**Files:**
- Modify: `src/vm/explore.rs`
- Modify: `src/ir/lift.rs` only if a narrowly scoped test utility is required

**Interfaces:**
- Consumes: `Emulator::run_retaining` and `Event::append_roots` from Tasks 2–3.
- Produces: `fn run_retaining_entry_params(emu: &mut Emulator<'_>, start: u64, budget: usize, entry_params: &mut [(Reg, Ref)]) -> Stop`, used by CFG exploration so `entry_params` are updated after every emulator compaction.

- [ ] **Step 1: Add failing explorer regression**

Add `compaction_preserves_overwritten_block_entry_parameters_for_grafting`. With the minimal executable in-memory PE, create block parameters, overwrite the corresponding state slots before the run, add enough dead history to trigger compaction, call the entry-parameter adapter, and assert every copied-back parameter remains structurally identical and can be grafted into a fresh CFG arena without an invalid ref or panic.

- [ ] **Step 2: Run the explorer regression and verify RED**

Run: `cargo test --release compaction_preserves_overwritten_block_entry_parameters_for_grafting -- --nocapture`

Expected: the test fails because exploration calls `run` without lending `entry_params` to compaction.

- [ ] **Step 3: Route mutable entry refs through `run_retaining`**

Implement `run_retaining_entry_params`: collect refs from the mutable `(Reg, Ref)` slice, call `run_retaining`, and copy remapped refs back before returning the stop. Make `Explorer::recover` keep `entry_params` mutable and call this adapter before any graft. Replace the duplicated event match in `divergence_roots` with `Event::append_roots` so diagnostics and production use one event-root definition.

- [ ] **Step 4: Run explorer and full regression suites**

Run: `cargo test --release compaction_preserves_overwritten_block_entry_parameters_for_grafting -- --nocapture`

Run: `cargo test --release`

Expected: the new regression and complete suite pass with zero failures.

- [ ] **Step 5: Commit explorer integration**

```powershell
git add src/vm/explore.rs src/ir/lift.rs
git commit -m "fix: retain block parameters across arena compaction"
git push
```

### Task 5: Static and repository-wide verification

**Files:**
- Modify only files needed to fix failures introduced by Tasks 1–4.

**Interfaces:**
- Consumes: completed implementation from Tasks 1–4.
- Produces: a clean release build and test result suitable for sample validation.

- [ ] **Step 1: Format and inspect the implementation diff**

Run: `cargo fmt --all -- --check`

If it fails, run `cargo fmt --all`, inspect the changed files, and rerun the check.

Run: `git diff --check`

Expected: no whitespace errors.

- [ ] **Step 2: Run release verification**

Run: `cargo test --release`

Run: `cargo build --release`

Run: `cargo clippy --release --all-targets -- -D warnings`

Expected: tests and build pass; clippy emits no new warning. If the repository has pre-existing clippy warnings, capture the baseline from commit `083a1d6` and demonstrate that the warning set is unchanged rather than suppressing it.

- [ ] **Step 3: Review unsafe boundaries and all `Ref` owners**

Use `rg -n "Ref|HashMap<Ref|Vec<\(Ref|Option<Ref" src/ir/lift.rs src/vm/state.rs src/vm/explore.rs` and account for every evaluator-owned reference that survives an instruction boundary. Add a failing regression before fixing any newly found omission.

- [ ] **Step 4: Commit verification-only corrections, if any**

Commit only if Step 1–3 required code changes, with a message describing the concrete correction, then push.

### Task 6: Cross-sample and DFS64 corpus validation

**Files:**
- Create outside Git: `C:\Users\31513\Desktop\ACE\work\arena-live-compaction-20260926\` for baseline/candidate JSON, logs, process exit records, elapsed time, and peak working set.
- Create: `docs/validation/2026-09-26-arena-live-node-compaction.md` containing commands, commit IDs, aggregate comparisons, and links/paths to retained artifacts.

**Interfaces:**
- Consumes: release binary from Task 5 and the recorded sample entries in the spec.
- Produces: reproducible evidence satisfying every acceptance rule before merge or upstream PR.

- [ ] **Step 1: Freeze binaries and baseline metadata**

Record `git rev-parse HEAD`, `rustc -Vv`, `cargo -V`, input SHA-256 hashes, environment overrides, and the exact CLI arguments. Build the candidate once with `cargo build --release`; do not rebuild between sample cases.

- [ ] **Step 2: Run the six cross-module cases in isolated processes**

Run `target\release\tvm-devirt.exe unresolved <sample> <entry> --json <output> --steps 400000 --max-blocks 512 --timeout 600` for ACE-Service64 `0x1400059f0`, ACE-CSI64-s2 `0x18006cee0`, ACE-Tray `0x140013cc0`, SGuardAgent64 `0x18000c0e0`, and DFS64 `0x180093320` plus `0x180017360`. Capture exit code, stdout/stderr, elapsed time, and peak working set for every process.

Expected: valid JSON; no crash, panic, OOM, or new CFG issue; the first four match their recorded block/unresolved/step baselines unless a recorded compaction explains a recovery change.

- [ ] **Step 3: Compare limit-trigger behavior against controls**

For each cross-module case, run once at the default limit and once with a deliberately lower `TVM_NODE_LIMIT` that triggers compaction without making the live DAG exceed the limit. Confirm all non-compacting paths are byte-for-byte identical after normalizing elapsed-time-only fields, and every changed path is associated with a limit crossing and remains CFG-valid.

- [ ] **Step 4: Run all 157 DFS64 partial functions**

Derive the entry list from the saved per-function/diagnostic artifacts under `C:\Users\31513\Desktop\ACE\work\ace-dfs64-s2`, launch one isolated process per function with bounded concurrency, and compare blocks, unresolved count and classification, total steps, timeout status, executable-target checks, and CFG issue categories against the frozen baseline. Retain all JSON and a machine-readable aggregate diff.

Expected: 157/157 processes produce valid JSON and no acceptance-rule violation; any recovery difference has an explicit compaction explanation.

- [ ] **Step 5: Run every historically Diverged function with an extended timeout**

Select all baseline records whose unresolved reason is folding divergence, rerun them with `--timeout 1800`, and record whether divergence disappears, moves, or remains because the compacted live DAG itself exceeds the limit. Inspect every changed terminator and CFG validation result.

- [ ] **Step 6: Run the full 425-function inventory when resources permit**

Use the saved DFS64 entry inventory and the same isolated-process runner. If machine resources prevent completion, document the exact completed count and reason; do not represent a partial run as full validation.

- [ ] **Step 7: Write and verify the validation report**

In `docs/validation/2026-09-26-arena-live-node-compaction.md`, include aggregate counts, every mismatch and disposition, peak-memory/time comparison for `0x180093320`, paths to raw artifacts, and the release verification outputs. Run `git diff --check` and verify all reported totals directly from the aggregate JSON.

- [ ] **Step 8: Commit and push validation evidence**

```powershell
git add docs/validation/2026-09-26-arena-live-node-compaction.md
git commit -m "docs: validate arena live-node compaction"
git push
```

### Task 7: Review, mirror integration, and upstream submission

**Files:**
- Modify implementation/tests/report only for review findings, with a failing regression for every behavioral correction.

**Interfaces:**
- Consumes: complete implementation and validation evidence.
- Produces: reviewed mirror change and a focused upstream PR linked to Issue #6.

- [ ] **Step 1: Request an independent whole-branch code review**

Review `083a1d6..HEAD` for missing roots, stale refs, identity changes, recursive traversal, mutation before validation, limit-loop behavior, and tests that do not prove their stated behavior. Fix each confirmed finding through RED/GREEN and rerun Task 5.

- [ ] **Step 2: Request a second review of sample evidence**

Check aggregate totals against raw JSON and acceptance rules. Any unexplained mismatch returns the work to the owning implementation task and blocks submission.

- [ ] **Step 3: Create and merge the mirror PR**

Open a PR from `codex/arena-live-compaction` to `NeuroSaki987/tvm-devirt-mirror:master`, include the issue, design, validation report, exact test commands, and sample totals. Merge only after required checks and reviews pass.

- [ ] **Step 4: Prepare the focused upstream branch and PR**

Rebase or cherry-pick only the core compaction implementation and its tests onto the current `jz0/tvm-devirt` default branch; exclude mirror-only diagnostic documentation. Rerun Task 5 and the six cross-module cases on that exact commit, then open the upstream PR with `Fixes #6` and attach the reproducible validation summary.
