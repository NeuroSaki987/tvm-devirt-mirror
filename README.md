# tvm-devirt-mirror

这是 [`jz0/tvm-devirt`](https://github.com/jz0/tvm-devirt) 的持续维护镜像，主要保留和发展上游尚未包含的恢复诊断能力。

反虚拟化、CFG 恢复、IR 和代码生成等基础功能来自原项目。这个仓库当前重点维护新增的 `unresolved` 命令，用来分析函数为什么没有完整恢复，并为后续恢复算法优化提供可复现数据。

## 相比原项目增加了什么

```bash
tvm-devirt unresolved input.exe 0x140001000
tvm-devirt unresolved input.exe 0x140001000 --json unresolved.json
tvm-devirt unresolved input.exe 0x140001000 --max-blocks 1024
```

`unresolved` 会重新恢复指定函数，并额外输出：

- 最终 CFG 中每个未解析块的具体原因；
- 间接跳转表达式、符号叶子和 0/1 候选条件；
- 候选取值后的目标、可执行属性及 VM 节归属；
- 小范围索引的枚举结果；
- CFG 结构校验，包括不可执行目标和 SSA 支配关系问题；
- 表达式 DAG 的节点组成、共享程度、活节点和死亡节点规模；
- block budget、超时、表达式膨胀与真实符号分支之间的区分。

诊断专属计算不计入恢复的超时预算，因此开启诊断不会仅仅因为统计工作较慢而改变恢复结果。工具也不会为了减少 `unresolved` 数量而强制吞掉无法证明不可达的分支。

常用参数：

```text
--steps <N>       单块指令步数预算
--max-blocks <N>  最大恢复块数，默认 512
--timeout <SEC>   恢复超时，默认 120 秒
--json <PATH>     将完整报告写入 JSON 文件
```

## 构建

需要 Rust 工具链；Windows 构建还需要 Visual Studio C++ 工具链。

```bash
cargo build --release
cargo test --release
```

其他基础命令及项目原理请参考[上游仓库](https://github.com/jz0/tvm-devirt)。

## 维护关系

- 原项目：[`jz0/tvm-devirt`](https://github.com/jz0/tvm-devirt)
- 本维护镜像：[`NeuroSaki987/tvm-devirt-mirror`](https://github.com/NeuroSaki987/tvm-devirt-mirror)
- `master` 以原项目为基线，镜像专属功能在此基础上维护。
- 通用且适合上游的独立修复仍应拆成小型 PR 提交给原项目。

---

## English

This is a maintained mirror of [`jz0/tvm-devirt`](https://github.com/jz0/tvm-devirt). The base devirtualizer comes from upstream; this repository focuses on additional tooling for explaining incomplete recovery.

The added `unresolved` command reports final unresolved CFG blocks, failed indirect-branch expressions, candidate arms, bounded-index probes, CFG invariant violations, and expression-DAG composition. Diagnostic-only work is excluded from the recovery timeout, and the tool does not discard unproven paths merely to reduce the unresolved count.

```bash
tvm-devirt unresolved input.exe 0x140001000
tvm-devirt unresolved input.exe 0x140001000 --json unresolved.json
tvm-devirt unresolved input.exe 0x140001000 --max-blocks 1024
```

Build and test with:

```bash
cargo build --release
cargo test --release
```

See the [upstream repository](https://github.com/jz0/tvm-devirt) for the original project, its architecture, and the standard commands.
