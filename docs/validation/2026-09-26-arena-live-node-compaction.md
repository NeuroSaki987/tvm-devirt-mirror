# Arena live-node compaction validation

This report validates the implementation proposed in
[jz0/tvm-devirt#6](https://github.com/jz0/tvm-devirt/issues/6). The tested candidate is
commit `ab8ed64` (`fix: preserve expression order across compaction`) on top of the
implementation range beginning at `083a1d6`.

Later branch commits change only test commentary and this report; they do not change the
executable logic exercised by the frozen `ab8ed64` binary. The release suite and build
were repeated after the commentary correction.

## Frozen candidate

- Binary: `tvm-devirt-ab8ed64.exe`
- Size: 2,928,128 bytes
- SHA-256: `D150A7B7296A350301A223B0E6AC862CA3F973348A4862B6D61BC2BB757DDCA5`
- Rust: `rustc 1.98.1 (48a229cea 2026-09-01)`; LLVM 22.1.8
- Cargo: `cargo 1.98.1 (797e8a9bc 2026-08-05)`
- Host: `x86_64-pc-windows-msvc`

The release binary was built once and copied out of `target` before sample testing.
Raw artifacts are retained under
`C:\Users\31513\Desktop\ACE\work\arena-live-compaction-20260926\ab8ed64`.

## Static and unit verification

The final candidate passed:

```text
rustfmt --edition 2024 src/ir/expr.rs src/ir/lift.rs src/vm/state.rs src/vm/explore.rs src/vm/diag.rs src/cli/diag.rs
git diff --check
cargo test --release                 # 192 passed; 0 failed
cargo build --release
cargo clippy --all-targets --all-features --message-format=json
```

Repository-wide `cargo fmt --check` continues to report only pre-existing formatting in
`src/binary/pe.rs`, `src/ir/ir.rs`, and `src/ir/regalloc.rs`; all files changed by this
branch were formatted directly. Clippy diagnostics were normalized by primary file,
lint code, and message: baseline `083a1d6` and `ab8ed64` both contain 255 warnings, with
zero added and zero removed.

An independent review found that DFS-based renumbering could reverse the Ref order of a
commutative expression. That would break later hash-consing of the same expression. A
regression test first reproduced the failure. The final fix preserves the original Ref
order, which is already topological because every node is created after its children.
The new regression and the full 192-test suite pass.

## Inputs

| Module | SHA-256 |
| --- | --- |
| ACE-Service64 | `8FE1D8D4935B4CD572AC8EB3A46C188C83E1BFDA2259681098211E55286F76FE` |
| ACE-CSI64-s2 | `6FA39D1CA635DA07D435C5CB6A3A78079CD7CFD73D86E70CC2604856B4CED47B` |
| ACE-Tray | `102EDCFF1E233C51FC715DA854571B6CA42FCCD7F74478C6594A10A7484DD4B2` |
| SGuardAgent64 | `B74189DA0EBBB4C778A5208E2EA438D37AE207247C3DDC049F3198AEF7170104` |
| ACE-DFS64-s2 | `A0626B5C5DDE8C41BA485DB5C1D778E182CAE2752DF79545AE9579F490F1213B` |

Each normal run used:

```text
tvm-devirt-ab8ed64.exe unresolved <input> <entry> --json <output> --steps 400000 --max-blocks 512 --timeout 600
```

The runner enforced an additional 660-second process wall limit. Tests inherited no
`TVM_NODE_LIMIT` unless a section below states otherwise.

## Cross-module baseline comparison

All six isolated default-limit runs exited zero, emitted valid JSON, reported a clean CFG,
and exactly matched the `083a1d6` baseline after excluding the additive `compactions`
diagnostic field.

| Case | Blocks | Unresolved | Steps | Compactions |
| --- | ---: | ---: | ---: | ---: |
| ACE-Service64 `0x1400059f0` | 8 | 0 | 316 | 0 |
| ACE-CSI64-s2 `0x18006cee0` | 25 | 0 | 175,389 | 0 |
| ACE-Tray `0x140013cc0` | 64 | 18 | 1,342 | 0 |
| SGuardAgent64 `0x18000c0e0` | 57 | 0 | 228,460 | 0 |
| DFS64 `0x180093320` | 31 | 2 | 477,795 | 0 |
| DFS64 `0x180017360` | 30 | 1 | 471,392 | 0 |

An isolated peak-working-set rerun of DFS large measured 145,575,936 bytes in 2.282
seconds, versus 136,142,848 bytes in 2.554 seconds for the baseline capture. The roughly
6.9% peak difference is within single-run allocator/OS variation, and this path recorded
zero compactions on both builds.

## Forced-compaction stress

The same six cases were repeated with `TVM_NODE_LIMIT=10000`. All processes exited zero,
emitted valid JSON, and reported clean CFG validation. Service and Tray did not reach the
limit. CSI and Agent retained exactly the default CFG/result while exercising 20 and 40
compactions respectively.

| Case | Compactions | Retained nodes after compaction | Maximum live-DAG depth | Recorded divergence |
| --- | ---: | ---: | ---: | ---: |
| Service | 0 | n/a | n/a | 0 |
| CSI | 20 | 543-1,162 | 15-30 | 0 |
| Tray | 0 | n/a | n/a | 0 |
| Agent | 40 | 402-1,148 | 20-55 | 0 |
| DFS large | 789 | 232-10,001 | 12-124 | 2 |
| DFS small | 114 | 156-5,853 | 40-2,061 | 2 |

The two DFS stress runs deliberately force a limit below some live working sets. Their
remaining divergence is therefore expected: the evaluator continues only when both the
retained count fits the configured limit and the retained DAG depth is safe for the
existing recursive folding analyses.

## DFS64 corpus

The saved partial inventory contains 157 entries. The final binary ran 156 entries with
two-process bounded concurrency. Entry `0x18001c900` was run separately in a single
process because an earlier two-process run exceeded the runner's 660-second wall limit
under CPU contention. Its isolated final-candidate result was:

```text
exit=0; valid JSON; 55 blocks; 4 unresolved; 635,528 steps;
4 compactions; 4 recorded divergence events; clean CFG
```

This result also exactly matches the `083a1d6` semantic summary. Its compactions reduce
the arena from roughly one million historical nodes to the live closure, then retain a
conservative divergence when that closure has unsafe depth; the process no longer incurs
the stack overflow observed before the depth guard was added.

The merged aggregate contains 157 unique results, zero process/JSON failures, four
compactions, and four recorded divergence events; all four compactions and divergence
events belong to `0x18001c900`. CFG validation is clean for 143 entries. The other 14
retain exactly the same issue summary as their saved baseline (unidentified blocks,
cannot-reach-exit blocks, or nondominating parameters); no new issue category or count
appeared. The machine-readable aggregate and saved-baseline diff are
`dfs157-final-results.json` and `dfs157-saved-baseline-diff.json` in the artifact root.

All 16 entries whose saved baseline contained a folding-divergence stop are included in
this corpus and were also run separately with the CLI timeout extended to 1,800 seconds.
That rerun used the historical `4,000,000`-step budget documented by
`tools/70_unresolved_diag.py`, ran one process at a time because one process peaked near
12 GiB, and produced 16/16 valid, clean results with 59 compactions. Thirteen entries no
longer report folding divergence. `0x180017360` retains one and `0x18001c900` retains two
conservative divergence stops because the compacted live DAG remains too deep; their six
diagnostic divergence events all match compaction site/pass records. `0x1800ecde0`
reaches the 4,000,000-step budget without divergence. The merged evidence is retained as
`diverged-extended-4m-1800-results.json`.

Saved per-function JSON predates the exact baseline and contains unrelated historical
differences, so it is retained as provenance rather than treated as an exact regression
oracle. Exact `083a1d6` A/B runs were used for the two resource-heavy cases:

- `0x18001c900`: 55 blocks, 4 unresolved, 635,528 steps on both builds.
- `0x1800cd950`: 560 blocks, 56 unresolved, 1,143,308 steps on both builds.

`0x1800cd950` reached a transient working set of about 7.2 GiB in the final matrix before
any compaction was triggered. Trace diagnostics contain no compaction record, confirming
that this is an existing multi-branch peak rather than compaction-retained data.

The full filtered inventory in `entries_real.txt` contains the 425 entries stated in the
plan. It was not represented as completed: the exact 157-entry partial inventory, all 16
historical divergence entries, six cross-module controls, and six low-limit stress cases
form the completed validation set. The separate raw discovery list contains 564
candidates, including false positives. A full 425-entry run was not attempted after the
final matrix demonstrated a 7.2-GiB single-process peak.

## Disposition

No crash, panic, OOM, malformed JSON, stale-reference symptom, new CFG issue, or
unexplained exact-baseline semantic mismatch remains in the completed validation set.
The retained raw JSON, stdout/stderr logs, runner summaries, frozen binaries, and input
lists are stored beside the artifact paths named above.
