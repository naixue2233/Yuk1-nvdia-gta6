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
6. **`crates/victory-gpu/src/inplace.rs`、`shadow.rs:6009`、`host.rs:10928`** —— 上面
   那两个实验的钩子还留在树里，默认关闭：`VICTORY_INPLACE=2` 现在也会接管被
   「显存副本」规则判成 big 的 buffer（`(!big || inplace_big)`），并且拒绝与
   late wait / relay 同时开（`inplace::blocks_waiting`，会警告一次）；
   `VICTORY_WC_BAR_TRY=1` 在 `GPUUploadHeapSupported` 为假时也照样向驱动要一块
   CPU 可映射的 L1 堆。两者在本机都不赢，原因见下面那节。
7. **`dist/`** —— 单文件分发：带内嵌运行时、静态链接 CRT 的 `victory.exe`，
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

## CPU 侧复制是怎么做的

这一段是花掉帧数的部分，值得写清楚。

### 为什么非要有这次拷贝

游戏创建的 GPU 资源不是 D3D12 资源，是它自己 `XMemAlloc` 出来的内存，然后交给
GPU 用。`crates/victory-gpu/src/shadow.rs` 给每个被跟踪的资源维护两样东西：

- **staging** —— 一份 CPU 可写的共享 buffer，`host()?.shared_buffer(size)?`
  （`shadow.rs:6027`）。CPU 写的就是它，而且它是 write-combined，下面那个带宽
  天花板就是它决定的。
- **显存副本** —— `card_for(size)`（`shadow.rs:6031`）在显存里建的那份；脚本设
  `VICTORY_CARD_BUFFERS_MIN_KB=64` 时每个 buffer 都有，描述符指向它
  （`buffer.GetGPUVirtualAddress()`，`shadow.rs:6053`）。

所以每帧 GPU 要读的每个字节都得走：游戏内存 → staging → 显存；GPU 写过的每个
字节还得走回来。这就是 CPU 侧复制，45–90 ms 的一帧里有 20–40 ms 是它。

### 怎么知道哪些字节变了

- 给游戏内存的页挂写监视；一份提交去问哪些页变了（`crate::watch::ask_once`、
  `watch::soft_any`），拿回页/区间的位图。
- 对被监视页的第一次写会缺页，进程的 VEH（`crate::watch::on_write`）记下来。
  `crates/victory-gpu/src/safecopy.rs` 的存在就是为了让拷贝本身不必每页付一次
  系统调用：那是一段 `global_asm!`（`safecopy.rs:16`），里有
  `victory_safe_copy`（`rep movsb`）、`victory_safe_put`
  （`vpcmpeqd`/`vpmaskmovd`，只写有差异的 dword）、`victory_safe_merge`、
  `victory_safe_changed`（32 字节比较）；拷到一半撞上未映射页会落回同一个
  handler，按「短拷贝」返回，不会把进程带走。`read_here`
  （`host.rs:9689`）在 `safecopy::on()`（`safecopy.rs:380`）为真时用它，否则用
  `ReadProcessMemory`。
- 询问本身不便宜，而且是故意不并行的：每帧约 2,300 个被写页 × 约 1.4 µs ≈ 3 ms
  硬成本；上游量过并行询问反而更慢（`watch.rs:654`："12 to 18 ms where 9 were"）。

### 记录成三种步骤

`crates/victory-gpu/src/host.rs` 里的 `enum Step` 就是全部词汇：

| 步骤 | 是什么 | 谁记下来的 |
|---|---|---|
| `Copy(dst, src, len)` | 游戏自己发起的 buffer 拷贝，写进某个资源 | `list.rs:947 buffer_copy` → `host/place.rs:226 place_copy`，否则 `host.rs:7404 defer_copy` |
| `Fill(dst, src, len, FillTo)` | 游戏内存 → 命令表的 upload 内存（那些小的） | `host.rs:7437 fill_scratch`、`host.rs:7459 defer_fill_buffer`、`host.rs:7451 defer_constants` |
| `Refresh(Shadow, from, to)` | 把资源 host 副本的一段刷到最新，一份提交只刷一次，谁先要谁刷 | shadow 自己的跟踪 |

`defer_copy` 会试着把相邻两条合成一条：如果上一条 `Copy` 的终点正好是这一条的
起点（源和目的都对上），`continues(...)` 为真，且开了 `VICTORY_JOIN_COPIES`，
就把字节数续上去（`host.rs:7404`，计数在 `COPIES_JOINED`）。真实提交上几乎
不命中：日志自己那行是 `10 copies, of them 0 continue the one before`。

### 落地：`run_deferred`

`host.rs:7886 run_deferred(&self, batch, air, taken)` 遍历步骤：

- `Step::Copy` → `batch.fill()`、`batch.title_written_at(dst, len)`
  （`host.rs:8866`）、`copy_mapped(dst, src, len, true)`
  （`host.rs:9451`），然后 `crate::watch::wrote(dst, len)` 与
  `crate::shadow::send_copied(dst, len)`（`shadow.rs:767`），让同一帧里后面的
  命令表能看见这次写。
- `Step::Fill` → wide 模式下只入队（`batch.fills.push(...)`，字节在这里算，
  时间算在 batch 里）；否则就地 `copy_mapped(..., false)`。
- `Step::Refresh` → 进 `batch.refreshes`，真正的刷新在 `Batch::flush` 里做。
- 末尾 `preparing(0, us)` / `preparing(1, bytes)`（`host.rs:780`），就是帧线里
  `copies … ms … MB` 那两个数。

接着 `Batch::fill()`（`host.rs:9179`）把排队的 fill 摊到常驻线程池上：
`const FEW: usize = 96; const PART: usize = 64;` —— 少于 96 个就地做（开线程
更贵），超过就 `crate::shadow::wide_each(fills.len().div_ceil(64), ...)`
（`shadow.rs:1640`，跑在 `wide_threads()+1` 条线程上，`shadow.rs:1533`：24 核
→ 7）。`Batch::flush()`（`host.rs:9223` → `flush_all`，`host.rs:9304`）先刷
纹理，再做 span 过滤：`shadow.nothing_new(address, len)`（`shadow.rs:4306`）把
游戏可证明没碰过的区间直接扔掉 —— 每帧 7,000–9,700 个里扔掉 6,100–8,900 个 ——
剩下的在默认的 `VICTORY_WIDE_REFRESH`（≥4 逻辑核即为真）下走同一个池。

### 把字节送到卡上

`host.rs:4647 card_before(&self, declared, picked, flown)` 问
`crate::shadow::card_dirty(sent_as)`（`shadow.rs:653`）要这次提交送过的片段，然后
每个区间记一条 `list.CopyBufferRegion(&shadow.buffer, a, staging, a, (b - a) as u64)`
—— 本机每帧 313 个区间、8.6–11.5 ms，另有 `VICTORY_CARD_REHASH_RUNS=1` 合并的
hash pass（96 次 over 102 个片段）与 compare pass（357 次）。这个成本是命令
录制的成本，不是带宽：`card_dirty` 自己只有几个 take/upgrade/take。

### 把字节取回来

反方向是 `shadow.rs:1952 back_piece(...)`，用 `first_difference` /
`last_difference`（`shadow.rs:1369`，32 字节分块 + 尾部标量）在一个片段里找出
真正变化的那段，只把那段读回来。每帧 21–22 MB、9–19 ms，由 `wide_back()`
（`shadow.rs:1498`，除非 `VICTORY_WIDE_BACK=0`）摊到线程上。着色器写的是
write-combined 内存且 `VICTORY_WC_NO_BACK=1` 时，整片在更早的地方就被丢掉：
每帧 8–26 GB 留在卡上，不再往回过。

### 在本机的真实成本

| | 每帧 |
|---|---|
| `copy_mapped` 的字节量 | 62–93 MB |
| `copy_mapped` 的时间 | 15–40 ms（约 4.1 GB/s） |
| fills | 7,000–12,000 个 × 约 500 B，提交线程上 5.3–7.8 ms |
| 显存侧录制 | 6.6–13.6 ms，313 个区间 |
| copy-back | 21–22 MB，9–19 ms，在提交线程旁边 |
| refresh 过滤 | 15.4–19.9 ms，其中 6,100–8,900 个区间已知不拷 |

墙在写这一侧：CPU 写 write-combined 内存大约 **4.1 GB/s**，而显卡从共享 buffer
拉同样的字节是 **19.4 GB/s**（`VICTORY_INPLACE_PROBE=1`：64 MB 用 3.3 ms）。
这不是线程数的墙 —— 把一次拷贝切成 1 MiB 分给池子实测 4.06 GB/s，串行是
4.09 GB/s，所以那个补丁不在本树里。

### 试过不行的

- **把拷贝切宽**（`Step::Copy` 里按 1 MiB 分片给 `wide_each`）：4.06 GB/s vs
  串行 4.09 GB/s，回退。
- **`flush_all` 里的 `FEW` 256 → 8**（把那层「已知不拷」的过滤推到 worker）：
  refreshes 从 18.1 变成 25.8–38.5 ms，`one at a time … before shader writes`
  从 0.7 变成 4.3–4.9 ms，回退；原因写在那条常量旁的注释里。
- **`VICTORY_JOIN_COPIES=1`**：10 次拷贝里 0 次能和上一条相接，没得合并。
- **`VICTORY_CARD_STEPS=2`**（让卡自己做拷贝）：日志里计数是
  `0.00 fills, 0.00 buffer copies` —— `card_steps` 只挑被在飞写入挡住的步骤
  （`card_steps.rs:286 plan`，并且受 `const BETWEEN: u64 = 4 << 20` 限制），它
  消化的是等待，不是批量搬字节。
- **不拷贝、让卡原地读游戏内存**（`VICTORY_INPLACE=2`）与 **CPU 可映射的显存
  副本**（`VICTORY_WC_BAR=1`）：本机两条都输，数字见上表和
  `RESEARCH-cpu-copies.md` —— PCIe 随机读把 GPU 拖到每帧 341–366 ms；驱动报
  `GPUUploadHeapSupported=false`，手工构造的 L1 堆被 `E_INVALIDARG` 拒。
- **`VICTORY_WC_BAR_TRY=1`**（无视能力位硬建）：150 秒里被拒 3,385 次，全是
  `0x80070057`。

还没做、也没量：把 `victory_safe_changed` 放到 fills 前面，字节没变就整条不写。
这是唯一还剩的「减少字节量」而不是「重排字节」的思路，估计能省 66 MB 里的
30–50%，但这只是估计。

相关开关一览：`VICTORY_WIDE_FILLS`（默认开）、`VICTORY_PREPARE_WIDE`（关；
开了从 32 个 fill 起就并行）、`VICTORY_REFRESH_ONCE`、`VICTORY_LIGHT_REFRESH`、
`VICTORY_WIDE_BACK`（开）、`VICTORY_CARD_REHASH_RUNS`、`VICTORY_WC_NO_BACK`、
`VICTORY_JOIN_COPIES`、`VICTORY_JOIN_PARTS`、`VICTORY_COPY_ASK`（每次拷贝前先
`VirtualQuery`；关 —— 在游戏的大池子里每次约 2 ms）、`VICTORY_SYNC_FINE`、
`VICTORY_SPANS_QUICK`。

## 着色器：按程序选形态（水面黑灰斑）

一个 console wave64 的 compute 程序，在只跑 32-lane 波的宿主上有好几种降级形态：deep-skips
（`without skips, scalars followed`）、local waves、loose LDS、paired、以及 cool_fox 的
wave-wide。哪个形态先赢，原来只看它能不能翻译通过，不看是哪个程序——于是就出现了
「形态本身能跑，但某个程序用它算出来是错的」。

最明显的一例是水面：两个 64-lane 水程序

```
5dc3b25986622949   6f225b641c5b0708
```

被塞进 wave-wide 形式后，开阔水面是黑灰斑；把它们从这个形式里排掉，水面就是波浪加太阳
的反光路径（`naixue2233/Victory-naixue2233` 的 README，2026-11-27：
"the black and grey patches came from forcing the two water programs into the wave-wide
form. Left out, open water shows waves and the sun's path."）。同理，上游 issue #62 的
14 个 `CS_ShadowRenderTiled*` 的问题是 deep-skips 形式。

所以这里给了四个按程序选形态的开关，值是 `hash=` 那一列（16 位十六进制，逗号或空格分隔）：

| 开关 | 作用 |
|---|---|
| `VICTORY_WAVE_WIDE_NOT` | 这些程序不许用 wave-wide |
| `VICTORY_WAVE_WIDE_ONLY` | 只有这些程序可以用 wave-wide |
| `VICTORY_WAVE_PAIRS_NOT` | 这些程序不许用 paired |
| `VICTORY_WAVE_PAIRS_ONLY` | 只有这些程序可以用 paired |

实现：`crates/victory-shader/src/emit/mod.rs` 里 `with_program(hash, f)` 把当前程序的哈希放进
thread-local，`wave_wide_here()` / `pairs_here()` 在每个形态闸门前查列表；GPU 侧在
`crates/victory-gpu/src/pipeline.rs`（运行时翻译）和 `crates/victory-gpu/src/shader_cache.rs`
（`victory shaders` 预热）两处注入同一个哈希——两边必须一致，否则预热会把没过滤的翻译写进
缓存被运行时复用。开关名由 `crates/victory-shader/build.rs` 扫描源码生成，自动进 shader
cache key，所以加开关不需要动缓存版本号。

`scripts/play-3070.ps1` 默认就带上这两个水程序的排除（`-NoWaterFix` 关掉；等价于
`-Extra "VICTORY_WAVE_WIDE_NOT=5dc3b25986622949,6f225b641c5b0708"`）。要看每个程序到底试了
哪些形态，加 `VICTORY_WAVE_WHY=1`，stderr 会打 `WAVE64-RETRY <hash> <形态>: wave ...`，被
列表排掉的程序还会打一行 `WAVE-WIDE-NOT <hash>`。

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
