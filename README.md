# tvm-devirt — 可配置块预算分支

> 分支：`feat/configurable-block-budget`
>
> 维护镜像：[`NeuroSaki987/tvm-devirt-mirror`](https://github.com/NeuroSaki987/tvm-devirt-mirror)
>
> 原项目：[`jz0/tvm-devirt`](https://github.com/jz0/tvm-devirt)

这个独立分支为 `devirt` 和 `write-devirt` 增加 `--max-blocks <N>`，默认值仍为 512；同时把块上限造成的截断与规则级未解析分开统计，并输出未解析原因族。默认参数下保持原有行为。

```bash
tvm-devirt devirt input.exe 0x140001000 --max-blocks 4096 --output function.bin
tvm-devirt write-devirt input.exe output.exe --max-blocks 4096
```

该功能用于区分“预算截断”和“真实恢复失败”，方便分析大型 CFG。提高块预算并不保证函数可写回：ACE 样本测试中，25 个撞顶函数的 1,609 个截断块可以全部展开，但规则级未解析没有减少，因此本分支定位为**预算控制和诊断能力**。

## English

This branch adds configurable `--max-blocks` support to `devirt` and `write-devirt`, separates block-budget truncation from genuine unresolved blocks, and reports unresolved reason families. The default remains 512 and preserves existing behavior. Raising the limit expands a truncated CFG but does not guarantee successful lowering.

---

# Upstream README

A static devirtualizer targeting Tencent VM (TVM). Recovers virtualized control flow & attempts to lower guest behavior back to native x86.

Devirtualized DriverEntry of ACE-GAME.sys.

![Claude, write me a devirtualizer](img/devirt_entry.png)

## Build

```bash
cargo build --release
cargo test
```

## Usage

List virtualized entry points:

```bash
tvm-devirt entries input.exe --all
```

Inspect one function:

```bash
tvm-devirt cfg input.exe 0x140001000
tvm-devirt ssa input.exe 0x140001000
tvm-devirt lower input.exe 0x140001000
tvm-devirt devirt input.exe 0x140001000 --output function.bin
```

Devirtualize all recoverable functions, devirtualized code is appended at the end of .tvm0 section:

```bash
tvm-devirt write-devirt input.exe output.exe
```

Run `tvm-devirt --help` or `tvm-devirt <command> --help` for all options.

## TVM Overview

TVM replaces a native function with a trampoline into a VM dispatcher. The dispatcher executes native x86 handlers, but the protected function's architectural state lives in the VM context.

```mermaid
flowchart TB
    subgraph TOP[ ]
        direction LR

        subgraph TEXT[.text]
            ENTRY["<b>Function entry</b><br/>jump to VM entry"]
        end

        subgraph VM[.tvm0 section]
            direction LR
            CORE["<b>Shared dispatcher and handlers</b><br/>fetch bytecode - execute - resolve next handler"]
            CTX["<b>Guest context</b><br/>GPRs - RFLAGS - VIP"]
            CORE <--> CTX
        end
    end

    subgraph NATIVE[Native execution outside the VM]
        direction LR
        BOXED["<b>Boxed instruction</b><br/>execute original x86"]
        CALL["<b>Guest call</b><br/>invoke native target"]
        EXIT["<b>Guest return or tail call</b><br/>resume native execution"]
        BOXED ~~~ CALL ~~~ EXIT
    end

    TEXT --> VM
    VM <--> NATIVE

    style TOP fill:transparent,stroke:transparent
```

Some operations execute outside the VM. A **boxed instruction** runs its original x86 encoding after the VM restores the registers it needs, then returns its effects to the guest context. A **guest call** invokes a native target and resumes with the return value and Windows x64 ABI clobbers represented in guest state. Guest returns and tail calls leave the dispatcher entirely.

The context contains all sixteen guest general-purpose registers, including the guest `RSP`, plus packed guest flags. The exact context layout can vary, so the lifter identifies the guest register image by observing the VM's register save and restore behavior rather than assuming a single fixed address.

Conditional control flow is implemented through dispatch arithmetic. A handler extracts a guest flag or predicate, uses it to select the next virtual program counter, and computes the address of the next native handler. The devirtualizer symbolically evaluates that process and forks the evaluator when the selected path depends on an unresolved guest value.

## Project Layout

```text
src/binary/   PE parsing, disassembly, format constants
src/vm/       VM entry discovery, state, and CFG exploration
src/ir/       symbolic expressions, hashing, lifting, scheduling, and allocation
src/codegen/  x86 emission, control flow, operands, frame layout, and unwind data
src/cli/      inspection, recovery, formatting, and writeback commands
```

## Limitations

- Codegen quality is far from perfect and the output binaries are meant for analysis only.
- Unsupported or unresolved VM behavior can leave a function incomplete.

## Credits

- [k0mkc](https://k0mkc.hatenablog.com/archive/category/AntiCheatExpert)
- [Back Engineering Labs](https://back.engineering/blog/31/07/2026/)
