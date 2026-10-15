# mirror 恢复改进的独立移植与验证（PR #4 历史记录）

> 后续全量验证发现结构退化；本文的“完整”仅表示旧版自报 0 unresolved 和产出文件，不保证有效 CFG。默认新增两例均有既有 SSA 错误，不能继续算作有效完整恢复。当前状态见 `2026-10-15-recovery-regression-fixes.md`。

日期：2026-10-15。基线为 mirror master `562f769`；此处是本 PR 构建的实际测量，不引用研究副本的全量成绩作为 mirror 成绩。

## 收录范围

- 默认：`(A|c1)^c2` 的强制位正确性修复、已知 flag 位的 `x-(x^k)` 消去，以及符号宽值高字节提取修正。
- `TVM_EXTENDED_COMPARISONS=1`：已知零位 XOR 转 OR、已知位相等/大小比较、等式常数搬移（包含混合宽度的拒绝边界）。默认关闭，旧研究报告中该组合有结构指标代价。
- `TVM_BOOLEAN_CERTIFICATES=1`：共享布尔源的全部赋值认证，最多 6 源 / 64 赋值；不猜测 guest 字值、不读取未知内存补常量。默认关闭。
- `TVM_JOIN_CONSTANT_REPLAY=1`：完整第一遍图的全部前驱常量候选；再重放并检查全部原边、终结形状、tail-call 目标和每条入边的常量保持。验证失败保留原第二遍结果；使用剩余时间/工作预算。默认关闭。
- 独立 `tools/recovery_census.py`：记录 per_entry、机器码长度/哈希，比较基线时保留失败/新增地址和缺失入口。

未移植 `src/recovery/*`、残差 CLI 开关、零收益的局部单/双源位运算归约器，以及原研究副本的大型验证框架。mirror 没有 residual 引擎，因此工具只测实际 devirt 路线，不伪造双路线数据。

## mirror 三组对照

三个高级开关全关为默认；三个全开为 opt-in。相同样本、入口、预算：`--steps 400000 --max-blocks 512 --timeout 120`，外部测试保护上限 65 秒。数据格式为 blocks/unresolved/total_steps。

| 用例 | 入口 | master | PR 默认 | PR 高级全开 |
|---|---|---|---|---|
| service | `0x1400059f0` | 8/0/316 | 8/0/316 | 8/0/316 |
| csi | `0x18006cee0` | 25/0/175389 | 25/0/175389 | 25/0/175389 |
| agent | `0x18000c0e0` | 57/0/228460 | 57/0/228460 | 57/0/228460 |
| sg-af280 | `0x1400af280` | 7/1/436855 | 7/1/436855 | 7/1/436855 |
| sg-526f0 | `0x1400526f0` | 41/3/57257 | 17/0/71339 | 15/0/65831 |
| sg-2bd60 | `0x14002bd60` | 85/4/161894 | 57/0/185916 | 51/0/171926 |
| sg-b1250 | `0x1400b1250` | 19/0/111956 | 19/0/111956 | 11/0/82892 |
| core-control | `0x14004590c` | 7/0/30896 | 7/0/30896 | 7/0/30896 |
| sg-52a30 | `0x140052a30` | 60/1/127791 | 60/1/127791 | 39/0/119875 |
| sg-98f90 | `0x140098f90` | 34/1/70061 | 34/1/70061 | 29/0/96349 |
| sg-9ea30 | `0x14009ea30` | 362/2/1137225 | 361/1/1166667 | 367/0/1259573 |

11 个入口中：master 完成 5 个；PR 默认完成 7 个，新增 2 个；高级全开完成 10 个，相对 master 新增 5 个。所测原成功入口没有变为失败。四个控制样本的默认恢复计数相同；生成代码的字节是否相同不等于完整运行时语义证明。

`0x1400af280` 在 400000 默认预算下三组均未完成。单独将同入口三组预算都设为 2000000 后，master / 默认均为 29/0/937755，高级为 23/0/920078，均完成。预算没有偷偷随开关改变。该入口不计作 PR 新增成功。

高级新增的 `0x140052a30`、`0x140098f90`、`0x14009ea30` 另跑 unresolved，同 400000 预算，均为 0 unresolved、issues.clean=true、timed_out=false。分别为 39 / 29 / 367 块。`0x140098f90` 最终机器码长度 **1177 字节**：更正旧研究报告早期的 1169 字节，后者不是最终产物。

## 测试与限制

- `cargo test --release --locked`：224 passed / 0 failed。
- `cargo build --release --locked`：成功。
- `python -m unittest discover -s tools -p "test_*.py"`：2 passed。
- `git diff --check`：通过。
- census 工具真实运行烟测：默认配置 `0x1400526f0` 记录 complete=true，并生成含 per_entry 的 JSON。

没有运行受保护样本；没有全量重测 251 入口或移植研究框架的 13 项护栏。原研究中已知的高级比较结构代价、CORE 布尔认证 CPU 时间 +128% 不会因为移植而自动消失，故默认关闭。该数值来自研究副本，未作为本 PR 的性能测量。

本地 DSH 独立审查尝试在插件树加载阶段失败（cordis:include），没有独立代理审查通过结论；本 PR 为待进一步审查的候选。

## 使用

```powershell
$env:TVM_EXTENDED_COMPARISONS='1'
$env:TVM_BOOLEAN_CERTIFICATES='1'
$env:TVM_JOIN_CONSTANT_REPLAY='1'
target/release/tvm-devirt.exe devirt input.exe 0x140001000 --output recovered.bin
python tools/recovery_census.py --sample input.exe --out baseline.json
# 更改配置之后保留逐入口结果；缺失或退化的旧成功入口会返回非零
python tools/recovery_census.py --sample input.exe --baseline baseline.json --out candidate.json
```

关闭：移除相应变量或设为 `0`。`mirror-recovery-cases.json` 保存该表的每例计数、退出码、机器码长度和单轮耗时；样本/生成二进制没有提交到仓库。历史单因子记录仅解释候选选择，不能作为当前 mirror 全量验证证据。
