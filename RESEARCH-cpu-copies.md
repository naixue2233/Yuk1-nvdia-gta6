# CPU 每帧 66 MB 拷贝：树内已有机制清单与实验矩阵

对象：`D:\Downloads\3070-lab`（scarlett-fox 血统的副本，原目录 `D:\Downloads\3070` 不改动）
测量基线：RTX 3080 10 GB / Win10 19045 / Agility D3D12Core 1.620.2 / 13.8–15.6 fps、64–79 ms/frame、3700 draws

## 1. 64 ms 花在哪（实测，日志 `runs\3070-20261127-213722.log`）

| 项 | 数值 | 谁的线程 |
|---|---|---|
| `preparing` | 30–32 ms | 提交线程（含 copies、textures、heap passes） |
| └ `copies` | **15.6 ms / 66 MB** | 提交线程 + `wide_each` 7 路 |
| └ `textures` | 1.4–1.9 ms（全线程 16.7） | 旁路线程 |
| `copying back` | 9–11 ms / 22 MB | 旁路线程 |
| GPU 忙 | 12.7–24.6 ms | — |
| 剩下 ~34 ms | 标题自己的 Xbox 代码 / present / 等待 | 标题线程 |

结论：CPU 侧结构性浪费只有两笔，66 MB 的进（fill/copy）与 22 MB 的出（copy-back）；其余 CPU 时间不在我们能改的层里。

## 2. 树内已有的三套「消灭拷贝」机制（全是 env，默认关）

### 2.1 `VICTORY_INPLACE`（`crates/victory-gpu/src/inplace.rs`）
- 开关：`inplace.rs:87 fn level()` ← `std::env::var("VICTORY_INPLACE")`；1 = title 的 window，2 = title buffer 也吃。
- 机制：`inplace.rs:235 fn open()` → `ID3D12Device13::OpenExistingHeapFromAddress1(at, bytes)`（部分区）/ `ID3D12Device3::OpenExistingHeapFromAddress`（整区）/ `OpenExistingHeapFromFileMapping`（section）；再 `inplace.rs:263 fn place()` → `CreatePlacedResource(heap, offset, buffer_desc, COMMON, None)`。**结果：title 的物理内存直接被 GPU 当 buffer 读，没有 host 副本 → 不拷贝、不刷新、不回拷。**
- 入口：`crates/victory-gpu/src/shadow.rs:6000` `if !big && let Some(placed) = crate::inplace::place_for(key.0, len, false)`，`(placed, key, len, false)` 建 `Shadow::placed`。
- 预算：`VICTORY_INPLACE_MB`（默认 4096）。
- 日志（每 10 s，`inplace.rs:210`）：`title memory in place: {windows} windows {MB} MB, {buffers} title buffers {MB} MB (no copy, no refresh, no copy-back); turned down: …`
- **硬约束**：`inplace.rs:76-80` 原文 — 2 的前提是「没有任何尚未提交的 flight 会通过这种 buffer 写 title 内存」，即 `VICTORY_LATE_WAIT` 必须为 0；**不要把 2 和 late wait 一起开**。另有 `inplace.rs:33-36`：heap 站着时 title 不能 `VirtualFree`/`VirtualProtect`/解映射那些页（返回 error 87）。

### 2.2 `VICTORY_WC_BAR`（`crates/victory-gpu/src/shadow.rs:2198`）
- 开关：`v == "1"`。
- 机制：`shadow.rs:6007 if wc_bar() && is_write_combined(key.0, len) && let Some((buffer, data)) = wc_mapped_for(size)` → 这个 title buffer 的 host 副本直接是**卡上可被 CPU 映射的一块 buffer**，CPU 写它 = 写显存，不需要「CPU 侧副本 + 发送」。
- 前提：`is_write_combined()`（`shadow.rs:2127`）查 `static WRITE_COMBINED`（`shadow.rs:2098`），由 `pub fn title_memory_write_combined(at, len)`（`shadow.rs:2103`，`lib.rs:141` 导出）在 title 申请 `PAGE_WRITECOMBINE`(0x400) 时登记 —— **upload 堆就是 WC**，所以这条对齐我们每帧的 fill 目标。
- 门槛：`wc_mapped_for(size)`（`shadow.rs:2205`）`size >= 1 GiB` → None；`used + size > budget / 10 * 8`（显存只剩 20%）→ None。
- 配套已有：`VICTORY_WC_NO_BACK=1`（脚本已开，`_MIN_KB=1024`）—— 卡上 WC 副本被 shader 写过就不比较、不取回。

### 2.3 `VICTORY_PORT_CONSTANTS`（`crates/victory-gpu/src/port.rs`，脚本已开）
- `port.rs:75 fn on()`；只吃 4 GiB 以下：`wanted()` = `at + len <= (1<<32)`；单区上限 `port.rs:68 const MOST: u64 = 512 << 20`。
- 实测已生效：`1 regions of the title's graphics memory in place (185 MB) … per frame on the CPU: 0 fills read out of them`。

## 3. 关键阻塞点（本轮新发现）

`shadow.rs:5993`：

```rust
let big = cards() && (size >= card_limits().0 || card_all());
...
if !big && let Some(placed) = crate::inplace::place_for(key.0, len, false) {
```

`card_limits().0` 由 `VICTORY_CARD_BUFFERS_MIN_KB` 决定，而 `scripts/play-3070.ps1` 设的是 **64 KiB**（`shadow.rs:229` 注释：64 KiB 以下的就是「every title buffer」）。**因此除了 64 KiB 以下的小 buffer，所有 title buffer 都是 `big`，`VICTORY_INPLACE=2` 对它们根本不生效。**

而 66 MB/帧正是 10–13 次、每次 5–6 MB 的 `Step::Copy`（`title buffer copies recorded per frame: 10`），全部 ≥ 64 KiB → 全部 `big` → 全部已放在显存里，CPU 每帧把 title 的写入推过去，4.06 GB/s 是 PCIe BAR 写带宽墙。

所以要试「结构性」方案，必须同时把 `VICTORY_CARD_BUFFERS_MIN_KB` 抬到很大（让 buffer 不再进显存），`inplace 2` 才够得着它们 —— 代价是 GPU 改从 title 内存读（PCIe 读）。这就是矩阵里 `inplace2all` 的意义：它测的是「PCIe 读 vs PCIe 写」这个交换值不值。

## 4. 实验矩阵（`scripts\lab-matrix.ps1`，每个 150 s，日志 `runs\3070-lab-<exp>-*.out.txt`）

| 名称 | 追加 env | 测什么 | 预期 |
|---|---|---|---|
| `probe` | `VICTORY_INPLACE_PROBE=1` | 只出 census：哪些 title 内存开得动、多少 MB | 给出 66 MB 里可开占比 |
| `base` | — | 本构建的对照（需与旧日志对齐） | 13.8–15.6 fps |
| `wcbar` | `VICTORY_WC_BAR=1` | WC buffer 的 host 副本搬到显存由 CPU 直写 | copies 15.6 → 8–12 ms；若 `wb` 日志为 0 则显存余量不足 |
| `inplace1` | `VICTORY_INPLACE=1` | 只吃 window + 小 buffer，可与 relay 共存 | 小幅（几 ms），日志 `title memory in place:` 非零 |
| `inplace2` | `VICTORY_INPLACE=2;VICTORY_RELAY=0` | 验证第 3 节的阻塞：`!big` 会挡掉全部大 buffer | 预期 copies 基本不变（= 证明阻塞成立） |
| `inplace2all` | `+ VICTORY_CARD_BUFFERS_MIN_KB=8388608` | 强行让 buffer 留在 title 内存并被 GPU 直接读 | copies → 0；GPU 读变慢，净效果待测 |

运行（游戏必须退出）：

```powershell
cd D:\Downloads\3070-lab
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\lab-matrix.ps1 `
  -Game "D:\GTA6.Xbox.v1.0.2.1-CYBERLEEK\game_scarlett_master.exe" -Seconds 150
# 只跑子集：-Only probe,wcbar
```

判读规则：
- `inplace` 行出现 `title memory in place: … MB (no copy, no refresh, no copy-back)` 且 MB 数与 copies 的 66 MB 同量级 → 该机制真能吃掉拷贝。
- `turned down:` 后面的分类告诉你为什么没吃上：`view`、`refused by host`、`over VICTORY_INPLACE_MB`、`not private whole`。
- `wcbar` 是否触发看 `write-combined buffer's host copy is in card memory the CPU maps` 出现次数。
- 任何 `unhandled exception 0x` → 该组合不正确，弃用（inplace 2 + relay 属于此类）。

## 5. 不建议做的（已对账）
- fxceless/victory-fork 100 个 PR 里，性能相关的 #234/#307 我们脚本已等效全开；#240/#255 本树已有（`shadow.rs:1943 back_piece`、`shadow.rs:1369/1381 first/last_difference`）；#1424（并行拷贝）作者自己 A/B 更差且已 closed；#185、#92 可移植但只会把 copies 之外那 ~12 ms 削掉一部分，天花板 +25~33%。
- 所有 patch `git apply --check` 全部失败（基于 main，我们是 scarlett-fox），要手抄。

## 6. 交付物与状态
- 副本：`D:\Downloads\3070-lab`（827 文件 / 1.4 GB，含 `sdk\`、`apps\`、`crates\`）
- 实验构建：`target-lab\release\victory.exe`（`cargo build --release --bin victory --target-dir target-lab`），需把 `D3D12\D3D12Core.dll`（4,874,024 B）放它旁边，`lab-matrix.ps1` 会检查
- 启动器：`scripts\play-3070.ps1` 已加 `-Tag/-Extra/-Seconds/-ExeDir`（lab 专用，原目录脚本不动）
- 原目录 `D:\Downloads\3070` 本轮未改；其 `target\release\victory.exe` 仍是实验构建，原版备份在 `victory-serial.exe`（游戏退出后可覆盖回去）
