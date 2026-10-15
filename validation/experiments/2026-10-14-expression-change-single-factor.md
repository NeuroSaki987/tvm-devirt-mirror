# 历史研究记录（非本 PR 的 mirror 实测）

以下记录来自独立研究副本。其中 recovery-state、target/ 变体补丁及旧复现命令并未移植到 mirror；不要在本仓库执行末尾的回滚命令。当前可复现配置和 mirror 验证见 `2026-10-15-mirror-recovery-port.md`。

# 单因素对照:表达式改动拆成"正确性修复"与"精度优化"

日期:2026-10-14。基线 `053f44a`。被测改动是工作区未提交的 `src/ir/expr.rs`(332 增 4 删),
按 `reviews/assistant-select-correlation/2026-10-14-算法修改与二进制分析.md` §46 的要求拆分对照。

## 拆分方式

按 hunk 拆成两个可独立应用的补丁(`target/variantA.patch`、`target/variantB.patch`),**两者都不含测试
hunk**(该 hunk 同时覆盖三个特性,单独应用无法编译):

- **A(正确性修复)**:只有 `(A|c1)^c2 = (A & ~c1) ^ (c1^c2)` 那一个 hunk。原代码在 `c1` 是 `c2` 子掩码
  时漏掉"清除 A 中被强制置位的部分"。
- **B(精度优化)**:其余 10 个 hunk——常量 Select 的同谓词两臂计算、以及 XOR 已知位规范化
  (全已知零→OR、全已知一→AND)。

## 结果

| 变体 | 护栏 | `legacy_resolved` | `gap_resolved` | `new_failures` | `legacy_blocks_total` | 结构项 |
| --- | --- | --- | --- | --- | --- | --- |
| A | **13 ok / 0 regressed** | 133 / 99(不变) | 2 / 2(不变) | **0** | 1850 / 3081(不变) | 全部满足 |
| B | **8 ok / 5 regressed** | 133 / 99 | **2 → 7**(SGuard) | **0 → 6**(SGuard) | CORE **1430**(−420)/ 3193 | segment-base 2→3、undecodable 5→12、DFS64 wte 2→**19** |
| A+B(工作区全量) | 8 ok / 5 regressed | 133 / 99 | 2 → 7 | 0 → 6 | 同上 | 同上 |

全量改动与 B 的结果逐项一致,说明 **A 既不带收益也不带回归**;收益(-5 个缺口入口)与全部 5 项回归
**都出自 B**。

## 结论

1. **正确性修复可以单独落地。** 它零行为代价,而那条规则已被独立反例证明是错的
   (`A=0x40, c1=0x40, c2=0x60` 时真值 `0x20`、旧值 `0x60`)。用保留旧数字来维持错误规则不可接受,
   现在有了"单独修它不影响任何指标"的实测依据。
2. **精度优化是一个混合体**:它第一次真的推动了 `gap_resolved`(SGuard 2→7),同时也制造了
   SGuard 6 个新失败、CORE 少 420 个块,以及三项结构回归。按验收准则,`gap_resolved` 上升算通过、
   `new_failures` 上升算打回,两条同时命中,所以**整体不可直接采用**,必须继续拆。
3. **下一步的单因素拆分**:B 内部还有两个独立成分——(B1)常量 Select 同谓词相关化简、
   (B2)XOR 已知位规范化。把它们再拆一次,确定 +5 与 5 项回归分别来自哪一个。若能分开,
   有益的一侧即可保留,有害的一侧单独定位。

## 复现

```
git archive 053f44a | ...            # 或者 git checkout -- src/ir/expr.rs
git apply target/variantA.patch      # 或 variantB.patch
cargo build --release --locked
python tools/regression_check.py     # 需要 TVM_Z3_LIBRARY
```

`target/guard_variantA.txt`、`target/guard_variantB.txt` 是两次完整护栏日志;
`target/guard_exprchange.txt` 是全量改动的日志。工作区已恢复为全量未提交改动(332 增 4 删)。

## 第二次拆分:B 内部的两个成分

拆分按**依赖边界**而不是行号位置,这是唯一能保证两侧都可单独编译的切法:

- **B1(谓词关联)**:`constant_select` 辅助函数、`select` 的同谓词自相关、以及把同一谓词分配到
  一元/二元运算上的规则,共 7 个 hunk。B1 拥有辅助函数,所以二元 `paired` 分配必须留在 B1。
- **B2(折叠算术恒等式)**:两处 `BinOp::Sub` 消去(`x-(x^k)` 与 `(A&B)-A`)与 `BinOp::Xor`
  已知位规范化,共 3 个 hunk。三者只用既有辅助函数,与 B1 无依赖,这才是两侧可独立测量的原因。

补丁为 `target/variantB1.patch`、`target/variantB2.patch`。

| 变体 | 护栏 | `gap_resolved` CORE / SGuard | `new_failures` | 结构项 |
| --- | --- | --- | --- | --- |
| B1 谓词关联 | **12 ok / 1 regressed** | 2 / 2(**不变**) | 0 / 0 | CORE `legacy_blocks_total` 1850 → **1426** |
| B2 折叠恒等式 | **9 ok / 4 regressed** | 2 / **7**(+5) | **0 / 6** | segment-base 2→3、undecodable 5→12、DFS64 wte 2→**19** |
| B1+B2 (= B) | 8 ok / 5 regressed | 2 / 7 | 0 / 6 | 上述全部 |

## 结论

1. **+5 个缺口入口全部出自 B2。** B1 对 `gap_resolved` 零贡献:CORE 2→2、SGuard 2→2,
   两条路线的缺口集合一个都没动。B2 单独就能复现 B 的全部收益,不需要 B1 参与。
2. **B1 是纯成本。** 它唯一的实测效果是让 CORE `legacy_blocks_total` 从 1850 掉到 1426,
   即少建 424 个块,而且不换来任何缺口收益。按验收准则这一项已经构成打回,因此
   **B1 可以直接弃用**,不必再拆。
3. **5 项回归里 4 项出自 B2,1 项出自 B1。** 两组的样本命中面不同:B1 只影响 CORE 块数,
   B2 同时影响 CORE 入口族、site extent、DFS64 与 SGuard 失败集。这说明 B2 的三条恒等式
   是全局性的值精度改动,而不是局部模式。
4. 因此下一步只在 B2 内部继续:三条恒等式互为独立 hunk,拆成 B2a(`x-(x^k)`)、
   B2b(`(A&B)-A`)、B2c(XOR 已知位规范化)分别单独测。目标是把 +5 与 4 项回归分开归属。

`target/guard_variantB1.txt`、`target/guard_variantB2.txt` 是两轮完整护栏日志。
每次运行都从提交态开始、只应用一个变体,所以两次之间唯一的差异就是被测变体。

## 第三次拆分:B2 的三条恒等式,以及 XOR 规范化的两个方向

B2 的三个 hunk 互不依赖,各自单独应用。B2c(XOR 已知位规范化)内部还有两个方向,再拆一次:
`known_zero(a) & k == k` 时 `a^k` 只能置位(→ `Or`),`known_one(a) & k == k` 时 `a^k` 只能清位
(→ `And`)。两者是同一个 `if` 里的两条分支,按行切片拆开。

| 变体 | 护栏 | SGuard `gap_resolved` | `new_failures` | CORE `legacy_blocks_total` |
| --- | --- | --- | --- | --- |
| B2a `x-(x^k)` | **13 ok / 0** | 2 → **4**(+2) | 0 | 1850(不变) |
| B2b `(A&B)-A` | **13 ok / 0** | 2(不变) | 0 | 1850(不变) |
| B2c_or `known_zero → Or` | 9 ok / 4 regressed | 2 → 4(+2) | **0 → 2** | 1855 |
| B2c_and `known_one → And` | **13 ok / 0** | 2 → **4**(+2) | 0 | 1850(不变) |
| B2c(两方向合一) | 9 ok / 4 regressed | 2 → **7**(+5) | **0 → 6** | 1855 |

## 结论(B2 拆分)

1. **B2c 的 4 项回归全部来自 Or 方向。** `known_zero → Or` 单独就已触发
   `core_entry_census`、`site_extent_consistency`、`dfs64_census` 与 SGuard 失败集四项;
   `known_one → And` 单独是 13/13、零回归,和 B2a 一样只带回 +2 而没有任何代价。
   同一个 `if` 的相邻两条分支,代价却完全不对称,所以"规范化 XOR"不能作为一个整体评价。
2. **B2b 完全惰性**:单独应用时 13/13,且 `gap_resolved`、`legacy_blocks_total`、
   入口族、undecodable、DFS64 全部逐项不变。它是一条正确的恒等式,但没有测到任何效果。
3. **B2a 与 B2c_and 各自 +2**,而 B2c 两方向合一是 +5,说明两者恢复的入口不完全重叠,
   也说明 Or 方向额外贡献的那部分只在"两个方向都存在"时才出现。
4. 因此 B1、B2b、B2c_or 三项都无实测价值或只有代价;保留候选是 **A + B2a(+ B2c_and)**。

## 交付候选的实测

`target/candA_B2a.patch` = 正确性修复 A + `x-(x^k)`,是一个可单独应用的补丁:

| 指标 | 基线 | A + B2a |
| --- | --- | --- |
| 护栏 | 13 ok / 0 | **13 ok / 0** |
| SGuard `legacy_resolved` | 99 | **101** |
| SGuard `gap_resolved` | 2 / 14 | **4 / 14** |
| CORE / SGuard `new_failures` | 0 / 0 | **0 / 0** |
| CORE `legacy_blocks_total` | 1850 | 1850 |
| 新增恢复的缺口入口 | — | `0x14002bd60`、`0x1400526f0` |

这是本次唯一一个"指标全部不动、同时多恢复两个入口"的组合:A 单独是纯正确性修复不带收益,
B2a 单独带回 +2 但当时没有与 A 一起测过,所以两者合起来又单独跑了一轮护栏。
`gap_resolved` 计数与逐条目地址取自 `target/artifacts_candA_B2a/dual_route_sguard_census.json`。

### 加上 B2c_and 不叠加

因为 B2a 与 B2c_and 各自都是 +2,而 B2c 两方向合一是 +5,看上去两者恢复的入口可能不同,
所以又测了 A + B2a + B2c_and(`target/candA_B2a_and.patch`):

| 组合 | 护栏 | SGuard `legacy_resolved` | SGuard `gap_resolved` | 恢复的缺口入口 |
| --- | --- | --- | --- | --- |
| A + B2a | 13 ok / 0 | 101 | 4 / 14 | `0x14002bd60`、`0x1400526f0`、`0x1400af280`、`0x1400afc80` |
| A + B2a + B2c_and | 13 ok / 0 | 101 | 4 / 14 | 同上,完全相同 |

两者恢复的是**同一批入口**,B2c_and 在这个组合里没有增量,只多两行规则。所以最终交付取
最小的那个:**A + B2a**。B2c 两方向合一之所以是 +5,是因为多出来的三只在 Or 方向同时存在时
才出现,而 Or 方向自带四项回归,不能作为交付的一部分。

## 交付状态

工作区的 `src/ir/expr.rs` 已按上述结论精简,精简脚本是 `target/build_candidate.py`:

| 项 | 全量未提交改动 | 精简后交付 |
| --- | --- | --- |
| `expr.rs` 改动 | 332 增 4 删 | **92 增 3 删** |
| 新增测试 | 12 个 | 4 个(只保留 A 与 B2a 对应的) |
| Rust 测试 | 277 通过 | **269 通过** |
| 护栏 | 8 ok / 5 regressed | **13 ok / 0 regressed** |
| SGuard `legacy_resolved` | 101(但 6 个新失败) | **101,`new_failures` 0** |
| SGuard `gap_resolved` | 7 / 14 | 4 / 14 |

被丢弃的每一片都不是删除而是**移出交付**:`target/variantB1.patch`、`variantB2b.patch`、
`variantB2c.patch`、`variantB2c_or.patch`、`variantB2c_and.patch`、`candA_B2a_and.patch`
都可随时单独 `git apply` 回去;全量改动的完整备份在 `target/save_fulldiff.u8.patch`
(以及 `ACE/tvm-devirt-workspace/scratch/` 下的同名副本)。

测试 hunk 无法整体应用——它同时覆盖三个特性,对精简后的文件无法编译。所以它由
`build_candidate.py` 从自己的新增行里重建,只保留覆盖被保留特性的那 4 个。丢掉测试的前提是
它测的代码也一起被丢掉了;保留下来的两条规则都带着自己的测试。

## 复现

```
git checkout -- src
git apply --recount --ignore-whitespace target/candA_B2a.patch   # 或 variantB2*.patch
cargo build --release --locked
python tools/regression_check.py                                  # 需要 TVM_Z3_LIBRARY
```

精简后的交付状态还可以直接用 `python target/build_candidate.py` 从提交态重现
(它会先应用 `candA_B2a.patch`,再按锚点插入测试,最后断言补丁不是"只有测试")。

`target/guard_variantB2a.txt`、`guard_variantB2b.txt`、`guard_variantB2c.txt`、
`guard_variantB2c_or.txt`、`guard_variantB2c_and.txt`、`guard_candA_B2a.txt`、
`guard_candA_B2a_and.txt`、`guard_deliverable.txt` 是各轮完整日志;
`target/artifacts_<变体>/` 里是按变体归档的 census 文档(含 `per_entry` 逐条目行
与 `gap_resolved_entries`/`new_failure_entries`)。

> 复现时的两个坑:`notes/cargo-env.ps1` 以 `exit $LASTEXITCODE` 结尾,点源会终止调用方脚本;
> 切换变体只能 `git checkout -- src`,用 `git checkout -- .` 会把 `tools/` 下未提交的护栏改动一起冲掉
> (本轮就这样丢过一次护栏改进,只能重写)。
