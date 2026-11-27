# Yuk1-nvdia-gta6

派生自 [Yaekai/victory-scarlett-3070](https://github.com/Yaekai/victory-scarlett-3070)
（分支 `scarlett-fox`）：用 D3D12 重新实现 Scarlett（Xbox Series）的图形通路，让
Scarlett 版本的游戏在你自己的 PC 上、用你自己那份拷贝跑起来。

这个分支额外解决两件事：在一台出厂 D3D12 运行时太老、装不下它自己生成的着色器的
机器上把启动链救回来；以及把结果打成单个 exe。

仓库里没有任何游戏数据，也不会提交：游戏本体由运行时的人自己提供（见
`docs/asset-policy.md` 与上游 `cargo xtask policy` 校验的那套 `.gitignore` 规则）。

## 和上游的差别

1. **`apps/victory/src/sdk.rs`（新增）** —— 导出 `D3D12SDKVersion = 620` 和指向
   `.\D3D12\` 的 `D3D12SDKPath` **指针**，让系统 `d3d12.dll` 的 loader 去装载
   Agility 运行时；`sdk::prepare()` 在第一次 D3D12 调用之前把内嵌的
   `D3D12Core.dll` 写到 `<exe>\D3D12\`（不可写时退到
   `%LOCALAPPDATA%\victory\d3d12\620\`）；系统版本低于 10.0.18362 或设了
   `VICTORY_D3D12_SDK=0` 时退回系统运行时。
2. **`apps/victory/build.rs`（新增）** —— `/EXPORT:D3D12SDKVersion,DATA` 与
   `/EXPORT:D3D12SDKPath,DATA`（`bin` 目标默认不导出符号），并把运行时拷进
   `OUT_DIR` 供 `include_bytes!` 内嵌。
3. **`apps/victory/src/main.rs`** —— `main` 第一句 `sdk::prepare()`。
4. **`crates/victory-gpu/src/pipeline/host.rs`（`root_signature`）** —— 继续按
   `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED`（0xc00）
   要根签名，但遇到 `E_INVALIDARG` 时打警告并**去掉这两个标志重试**，而不是把
   所有管线一起丢掉。
5. **`scripts/play-3070.ps1`** —— 新增 `-Relay` / `-LandThreads` 参数
   （`VICTORY_RELAY`、`VICTORY_LAND_THREADS`）。
6. **`dist/`** —— 单文件分发：带内嵌运行时、静态链接 CRT 的 `victory.exe`，
   加上 `Play-GTA6.cmd`、`victory.env.cmd`（74 个 `VICTORY_*` 开关）、
   给接收方的 `README.txt`。

## GPU 这一侧是怎么实现的

游戏是 Scarlett 构建：它调 Xbox GDK（`XMemAlloc`、虚拟盘、dstorage、display /
lifecycle），发的是 Xbox 的 D3D12 命令表和着色器。这里没有硬件模拟：
`victory-hle` 在本进程里应答 GDK 调用，`victory-loader` 把游戏自己的 exe 载进来，
`victory-gpu` 把游戏对 GPU 的要求翻译成 PC 上真实的 D3D12 调用，跑在 Agility
运行时之上。决定帧率的是下面三件事。

### 一、着色器与根签名

游戏的 DXIL 假定 Shader Model 6.6 的堆直索引，所以本渲染器建的每个根签名都带
`CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED`（0xc00），
并以 `D3D_ROOT_SIGNATURE_VERSION_1_1` 序列化
（`crates/victory-gpu/src/pipeline/host.rs`、`crates/victory-shader/src/emit/host.rs`）。

Windows 10 自带的 `d3d12.dll`（10.0.19041）对这两种标志的回答是
`Unsupported bit-flag set (root signature flags c00)`，随后每次
`CreateRootSignature` 都得 `E_INVALIDARG`：管线全丢，`victory-image` 在
`+0x68` 解引用空指针，游戏以 `0xc0000005` 退掉。这正是本分支修的那条链：
Agility 运行时实现了 6.6；拿不到运行时的时候，重试把标志去掉，而不是把管线全扔掉。

### 二、游戏的内存就是本进程的内存，显卡拿到的是它的影子

游戏创建的每个 GPU 资源都活在本进程自己的内存里。`shadow.rs` 为它分配一份 CPU
可写的 *staging*（就是 `shadow.rs:6027` 里那块 write-combined 共享 buffer），
资源够大时再在显存里放一份 *card-side* 副本（`card_for`，`shadow.rs:6031`；
脚本设 `VICTORY_CARD_BUFFERS_MIN_KB=64` 时等于「每个 buffer 都有」）。GPU 读的
描述符指向显存副本（`buffer.GetGPUVirtualAddress()`，`shadow.rs:6053`）。

也就是说：GPU 基本不直接读游戏内存，唯一的例外是渲染器端口原地映射的那 185 MB
（`port.rs`，`OpenExistingHeapFromAddress`）。

### 三、每帧由 CPU 把「变了的部分」搬过去

提交线程遍历记录下来的命令表，问写监视哪些页变了，只把变了的字节拷进 staging。
两条路：buffer 拷贝（`Step::Copy` → `copy_mapped`）和一堆小 fill（每帧
7,000–12,000 个，每个约 500 字节，分摊到 `wide_each` 的线程上）。然后
`card_before` 给每个变化区间记一条 `CopyBufferRegion` 把 staging 推上显存
（每帧 313 个区间），提交；之后再把着色器写过的 21–22 MB 拷回游戏内存 —— 除非
`VICTORY_WC_NO_BACK=1` 把 write-combined 的写留在卡上。

**这条拷贝就是 PC 上的墙，而且是内存类型的墙，不是线程数的墙**：CPU 写
write-combined 内存约 **4.1 GB/s**（每帧 66–90 MB → 15–40 ms），而显卡从共享
buffer 拷同样的字节是 **19.4 GB/s**（`VICTORY_INPLACE_PROBE=1` 实测：64 MB 用
3.3 ms）。加线程没有用 —— 同样 66 MB，7 线程 16.3 ms，跟单线程串行一遍一样 ——
成本在字节量和屏障上，不在并行度。

### 异步

`VICTORY_RELAY=2` 让一份提交在飞的同时准备下一份；`VICTORY_LAND_THREADS`
（默认 4，我们跑 8）负责把完成的片段落地。8 线程让 landing 从 16.4 降到
13.2 ms、merge 从 6.4 降到 5.3 ms，但帧率没动：提交线程自己还是每帧 39–46 ms，
GPU 有一半时间在后面闲着（日志里那句 `nothing of ours queued … the card idle
behind us`）。

### 一帧花在哪（RTX 3080 / Windows 10，17–22 fps，45–60 ms）

| 部分 | ms | 说明 |
|---|---|---|
| 拷进 staging 的 copies + fills | 16–25 | 62–70 MB，约 4 GB/s |
| 显存侧录制 + hash/compare passes | 6–13 | 313 个区间、96 次 hash pass |
| 纹理刷新 / detile | 2–5 | 27–34 MB |
| copy-back | 9–19 | 21–22 MB，在另一条线程 |
| 游戏自己的代码、present、等待 | ~34 | 这里动不了 |
| GPU | 4–25 | 剩下的 |

所以「把 CPU 侧全部变免费」的天花板是约 48 ms、20.8 fps：+33%，这是算术不是
悲观。两条结构性方案都试过，在这台机器上都输 —— 数字见
`RESEARCH-cpu-copies.md`：

- `VICTORY_INPLACE=2`（让卡直接原地读游戏 buffer）：拷贝确实归零，但 GPU 的
  PCIe 随机读把它推到**每帧 341–366 ms**（2.4 fps）；而且那里的写监视看不到
  着色器写，relay 必须一起关掉。
- `VICTORY_WC_BAR=1`（CPU 可映射的显存副本）：要求
  `D3D12_OPTIONS16.GPUUploadHeapSupported`，本机驱动报 false；手工构造
  `D3D12_HEAP_TYPE_CUSTOM` + `MEMORY_POOL_L1` 的堆在 150 秒里被 `E_INVALIDARG`
  拒了 3,385 次。ReBAR 是有的（BAR1 16 GiB），是运行时/驱动的组合没开这条路。

## 构建

`D3D12Core.dll` 1.620.2 就放在仓库里：
`sdk/agility/build/native/bin/x64/D3D12Core.dll`（4,874,024 字节，微软的 Agility
运行时可再分发件，说明见 `sdk/README.md`），克隆下来直接能编：

```powershell
cargo build --release --bin victory    # target/release/victory.exe
```

`scripts/fetch-d3d12-sdk.ps1` 只用于重新拉这个文件或换版本
（`-Version 1.619.6`）。不管哪个版本，都必须和 `apps/victory/src/sdk.rs` 里的
`SDK_VERSION`（620）对上。

单文件、不依赖 VC++ 运行时（运行时内嵌、启动时写出）：

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --bin victory --target-dir target-static
```

`sdk/` 里那份不在时也能编，只是可执行文件会退回系统运行时 —— 那正是它在
Windows 10 上渲染不了的那种情况，所以别删。

## 运行

```powershell
./scripts/play-3070.ps1 -Game "D:\path\to\game_scarlett_master.exe" -Relay 2 -LandThreads 8
```

或用打好的包：`dist\Play-GTA6.cmd "D:\path\to\game_scarlett_master.exe"`。

首次运行要翻译并缓存游戏的着色器（约 26,000 个，约 80 秒，峰值约 6 GB）：
`victory.exe shaders <游戏目录>`，然后 `victory.exe warm`。

## 实测

| 场景 | 帧 | 帧时间 | copies |
|---|---|---|---|
| 轻场景（约 2,000 draws） | 20–22 | 45–50 ms | 62 MB / 16 ms |
| 中等（2,400–3,700 draws） | 13–17 | 58–79 ms | 65–83 MB / 19–25 ms |
| 重场景（7,700–9,900 draws） | 10–13 | 76–97 ms | 82–93 MB / 23–40 ms |
| 8,000+ draws 极限 | 4.9–5.9 | 170–203 ms | 88–93 MB / 40 ms |

同一场景把 landing 从 4 线程换到 8 线程：landing 16.4 → 13.2 ms、merge
6.4 → 5.3 ms、等卡 29.5 → 25.3 ms，**帧率不变**（墙在提交线程）。

## 不要移植的东西

- [fxceless/victory-fork#1424](https://github.com/fxceless/victory-fork/pull/1424)
  （并行拷贝）是作者自己关掉的未合并 PR：他自己的 A/B 是 6.7 → 5.7 fps，而且
  4 MiB 的阈值在真实提交上根本不触发（真实拷贝 0.8–2.5 MB）。
- 把 `FEW` 从 256 调到 8（把那 9,000 次「已知不拷」的判定推到 worker 上）：实测
  refreshes 18.1 → 25.8–38.5 ms，更慢。有用功在那约 900 次真拷上，不在判定上。

`RESEARCH-cpu-copies.md` 记了机制清单、probe 输出、以及每个开关到底做了什么。

## 许可

沿用上游 `LICENSE`（GPL-3.0-or-later）。可执行文件里内嵌的 D3D12 Agility 运行时
是微软的可再分发件（`Microsoft.Direct3D.D3D12` 1.620.2 x64：
`sdk/agility/build/native/bin/x64/D3D12Core.dll`，按微软自己的条款，不是 GPL），
放在仓库里只是为了让克隆后不必再联网拉一次。
