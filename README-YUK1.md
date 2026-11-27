# Yuk1-nvdia-gta6

Derived from [Yaekai/victory-scarlett-3070](https://github.com/Yaekai/victory-scarlett-3070)
(branch `scarlett-fox`): a D3D12 re-implementation of the Scarlett (Xbox Series)
graphics path, so a Scarlett title runs on a Windows PC from the copy you own.
This version adds what was needed to make it start on a machine whose in-box
D3D12 runtime is too old for the shaders it emits, and to ship the result as a
single executable.

No game data is here, and none is ever committed: the title's own files are
supplied at runtime by whoever runs it (see `docs/asset-policy.md` and the
`.gitignore` rules that the upstream `cargo xtask policy` enforces).

Chinese version of this file, including the same GPU write-up: [README_zh.md](README_zh.md).

## What is different from upstream

1. **`apps/victory/src/sdk.rs` (new)** — exports `D3D12SDKVersion = 620` and a
   `D3D12SDKPath` *pointer* to `.\D3D12\`, so the in-box `d3d12.dll` loader
   loads the Agility runtime; `sdk::prepare()` writes `D3D12Core.dll` out of the
   binary before the first D3D12 call (to `<exe>\D3D12\`, else
   `%LOCALAPPDATA%\victory\d3d12\620\`), and steps aside for the in-box runtime
   on a build older than 10.0.18362 or with `VICTORY_D3D12_SDK=0`.
2. **`apps/victory/build.rs` (new)** — `/EXPORT:D3D12SDKVersion,DATA` and
   `/EXPORT:D3D12SDKPath,DATA` (a `bin` target exports nothing on its own), and
   the copy of the core into `OUT_DIR` for `include_bytes!`.
3. **`apps/victory/src/main.rs`** — `sdk::prepare()` first thing in `main`.
4. **`crates/victory-gpu/src/pipeline/host.rs`** (`root_signature`) — ask for
   `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED` (0xc00),
   and on `E_INVALIDARG` warn and retry without them, instead of losing every
   pipeline to a runtime that predates them.
5. **`scripts/play-3070.ps1`** — `-Relay` / `-LandThreads` parameters
   (`VICTORY_RELAY`, `VICTORY_LAND_THREADS`).
6. **`crates/victory-gpu/src/inplace.rs`, `shadow.rs:6009`, `host.rs:10928`** — the
   two experiments above are still in the tree, default off: `VICTORY_INPLACE=2`
   now also takes buffers the card-buffer rule marks big
   (`(!big || inplace_big)`), refuses to run with late wait / the relay
   (`inplace::blocks_waiting`, warned once), and `VICTORY_WC_BAR_TRY=1` asks the
   driver for a CPU-mapped L1 heap even when `GPUUploadHeapSupported` is false.
   Neither wins on this machine; the section below says why.
7. **`dist/`** — the single-file distribution: `victory.exe` with the runtime
   and a statically linked CRT, `Play-GTA6.cmd`, `victory.env.cmd` (74
   `VICTORY_*` switches), `README.txt` for whoever receives it.

## How the GPU side works

The title is a Scarlett build: it calls the Xbox GDK (`XMemAlloc`, virtual
drives, dstorage, display/lifecycle) and emits Xbox D3D12 command lists and
shaders. Nothing here emulates hardware; `victory-hle` answers the GDK calls in
this process, `victory-loader` brings the title's own executable in, and
`victory-gpu` translates what the title asks the GPU for into real PC D3D12 calls
on the Agility runtime. The three pieces that decide the frame rate:

**1. Shaders and root signatures.** The title's DXIL assumes Shader Model 6.6
with heap direct indexing, so every root signature this renderer builds carries
`CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED` (0xc00) and
is serialised as `D3D_ROOT_SIGNATURE_VERSION_1_1`
(`crates/victory-gpu/src/pipeline/host.rs`, `crates/victory-shader/src/emit/host.rs`).
An in-box Windows 10 `d3d12.dll` (10.0.19041) answers that with
`Unsupported bit-flag set (root signature flags c00)` and `E_INVALIDARG` for
every pipeline — which is the crash this fork fixes: the Agility runtime
implements 6.6, and if it is missing the retry drops the flags instead of the
pipelines.

**2. Title memory is host memory, and the card gets shadows of it.** A GPU
resource the title creates lives in the process's own memory. For each one
`shadow.rs` allocates a *staging* buffer that the CPU can write (that is the
write-combined shared buffer at `shadow.rs:6027`) and, when the resource is big
enough, a *card-side* copy in VRAM (`card_for`, `shadow.rs:6031`; with
`VICTORY_CARD_BUFFERS_MIN_KB=64` that is every buffer). Descriptors the GPU reads
point at the card copy (`buffer.GetGPUVirtualAddress()`, `shadow.rs:6053`). So the
GPU never reads title memory directly — except the 185 MB region the renderer
port places in place (`port.rs`, `OpenExistingHeapFromAddress`).

**3. Per frame, the CPU copies what changed.** The submitting thread walks the
recorded command lists, asks the write watch which of the title's pages changed,
and copies only those bytes into the staging buffers. Two paths: buffer copies
(`Step::Copy` → `copy_mapped`) and the small "fills" (7,000–12,000 a frame, ~500
bytes each, spread over `wide_each` threads). Then `card_before` records one
`CopyBufferRegion` per changed range to move staging → VRAM (313 ranges a frame),
submits, and later copies back the 21–22 MB the shaders wrote, unless
`VICTORY_WC_NO_BACK=1` leaves write-combined writes on the card.

That copy is the wall on a PC, and it is a memory-type wall, not a thread-count
one: the CPU writes write-combined memory at about **4.1 GB/s** (66–90 MB a
frame → 15–40 ms) while the card copies the same bytes out of a shared buffer at
**19.4 GB/s** (measured by `VICTORY_INPLACE_PROBE=1`: 64 MB in 3.3 ms). More
threads do nothing — the same 66 MB took 16.3 ms on 7 threads as it did in one
serial pass — because the bytes and the barrier are the cost.

**Asynchrony.** `VICTORY_RELAY=2` keeps one submission in flight while the next
is prepared, and `VICTORY_LAND_THREADS` (default 4, we run 8) lands the finished
pieces. 8 threads made landing 16.4 → 13.2 ms and merge 6.4 → 5.3 ms but did not
move the frame rate: the submitting thread is still 39–46 ms a frame, and the GPU
is idle behind it about half the time (`nothing of ours queued … the card idle
behind us`).

Where a frame goes on a RTX 3080 / Windows 10 (17–22 fps, 45–60 ms):

| part | ms | note |
|---|---|---|
| copies + fills into staging | 16–25 | 62–70 MB at ~4 GB/s |
| card-side recording + hash/compare passes | 6–13 | 313 ranges, 96 hash passes |
| texture refresh / detile | 2–5 | 27–34 MB |
| copy-back | 9–19 | 21–22 MB, on another thread |
| the title's own code, present, waits | ~34 | unchanged by anything here |
| GPU | 4–25 | what is left over |

So the ceiling for "make the CPU side free" is ~48 ms, 20.8 fps: +33%, and that
is arithmetic, not pessimism. Two structural answers were tried and both lose on
this machine — see `RESEARCH-cpu-copies.md` for the numbers:

- `VICTORY_INPLACE=2` (title buffers read by the card in place): copies go to
  zero, but the GPU's random PCIe reads push it to **341–366 ms a frame** (2.4
  fps), and the write watch cannot see shader writes there, so the relay has to
  be off as well.
- `VICTORY_WC_BAR=1` (CPU-mapped VRAM copy): needs
  `D3D12_OPTIONS16.GPUUploadHeapSupported`, which this driver reports false; a
  hand-built `D3D12_HEAP_TYPE_CUSTOM` + `MEMORY_POOL_L1` heap is refused with
  `E_INVALIDARG` 3,385 times in 150 s. ReBAR is present (BAR1 16 GiB); the
  runtime/driver combination is what does not offer it.

## The CPU-side copy

This is the part that costs the frames, so it is worth spelling out.

### Why a copy exists at all

A GPU resource the title creates is not a D3D12 resource: it is memory the title
allocated for itself (`XMemAlloc` arenas) that the title then hands to the GPU.
`crates/victory-gpu/src/shadow.rs` keeps two things for every tracked one:

- **staging** — a CPU-writable shared buffer, `host()?.shared_buffer(size)?`
  (`shadow.rs:6027`). This is the memory the CPU writes, and it is
  write-combined, which is what sets the bandwidth ceiling below.
- **the card copy** — a VRAM resource from `card_for(size)` (`shadow.rs:6031`),
  made for every buffer once `VICTORY_CARD_BUFFERS_MIN_KB=64` (the default in our
  launcher), with the descriptor pointing at it
  (`buffer.GetGPUVirtualAddress()`, `shadow.rs:6053`).

So per frame every byte the GPU reads has to travel title memory → staging →
VRAM, and every byte the GPU wrote has to travel back. That travel is the CPU
side copy, and it is where 20–40 ms of a 45–90 ms frame goes.

### Deciding what changed

- Pages of title memory are put under a write watch; a submission asks which of
  them changed (`crate::watch::ask_once`, `watch::soft_any`) and gets the answer
  as page/range bits.
- The first write to a watched page faults; the process's VEH
  (`crate::watch::on_write`) records it. `crates/victory-gpu/src/safecopy.rs`
  exists so that the copies themselves do not pay a syscall per page: it is a
  `global_asm!` block (`safecopy.rs:16`) with `victory_safe_copy` (`rep movsb`),
  `victory_safe_put` (`vpcmpeqd`/`vpmaskmovd`, writes only the dwords that
  differ), `victory_safe_merge` and `victory_safe_changed` (32-byte compare),
  and a fault inside them lands in the same handler and is reported back as a
  short copy instead of taking the process down. `read_here` (`host.rs:9689`)
  uses it when `safecopy::on()` (`safecopy.rs:380`), otherwise
  `ReadProcessMemory`.
- Asking is not free and is not parallelised on purpose: about 2,300 written
  pages a frame at ~1.4 µs each is ~3 ms of hard cost, and a parallel ask was
  measured slower upstream (`watch.rs:654`: "12 to 18 ms where 9 were").

### Recording the work: three step kinds

`enum Step` in `crates/victory-gpu/src/host.rs` is the whole vocabulary:

| step | what it is | recorded by |
|---|---|---|
| `Copy(dst, src, len)` | the title's own buffer copy into a resource | `list.rs:947 buffer_copy` → `host/place.rs:226 place_copy`, else `host.rs:7404 defer_copy` |
| `Fill(dst, src, len, FillTo)` | title memory into a list's upload memory — the small ones | `host.rs:7437 fill_scratch`, `host.rs:7459 defer_fill_buffer`, `host.rs:7451 defer_constants` |
| `Refresh(Shadow, from, to)` | bring a range of a resource's host copy up to date, once per submission, whichever list asks | the shadow's own tracking |

`defer_copy` tries to make one step out of two neighbours — if the previous step's
`Copy` continues exactly where this one starts (both source and destination),
`continues(...)` is true and with `VICTORY_JOIN_COPIES` the byte count is simply
extended (`host.rs:7404`, counters `COPIES_JOINED`). On real submissions this
almost never fires: the log's own line says
`10 copies, of them 0 continue the one before`.

### Running them: `run_deferred`

`host.rs:7886 run_deferred(&self, batch, air, taken)` walks the steps and:

- `Step::Copy` → `batch.fill()`, `batch.title_written_at(dst, len)` (`host.rs:8866`),
  `copy_mapped(dst, src, len, true)` (`host.rs:9451`), then `crate::watch::wrote(dst, len)`
  and `crate::shadow::send_copied(dst, len)` (`shadow.rs:767`) so later lists in the
  same frame see the write.
- `Step::Fill` → in wide mode it is only queued (`batch.fills.push(...)`, bytes
  counted here, time counted in the batch); otherwise `copy_mapped(..., false)`
  right away.
- `Step::Refresh` → `batch.refreshes`; the actual refresh work happens in
  `Batch::flush`.
- at the end, `preparing(0, us)` / `preparing(1, bytes)` (`host.rs:780`) feed the
  two numbers the frame line prints as `copies … ms … MB`.

Then `Batch::fill()` (`host.rs:9179`) spreads the queued fills over the kept-thread
pool: `const FEW: usize = 96; const PART: usize = 64;` — below 96 fills it does
them inline (threads would cost more), above that
`crate::shadow::wide_each(fills.len().div_ceil(64), ...)` (`shadow.rs:1640`, which
runs on `wide_threads()+1` threads, `shadow.rs:1533`: 24 cores → 7). `Batch::flush()`
(`host.rs:9223` → `flush_all`, `host.rs:9304`) does the texture refreshes and then
the span filter: `shadow.nothing_new(address, len)` (`shadow.rs:4306`) throws out
the ranges the title provably did not touch — 6,100–8,900 of 7,000–9,700 a frame —
and pushes the rest through the same pool when `VICTORY_WIDE_REFRESH`'s default
applies (≥4 logical CPUs).

### Getting the bytes onto the card

`host.rs:4647 card_before(&self, declared, picked, flown)` asks
`crate::shadow::card_dirty(sent_as)` (`shadow.rs:653`) for the pieces this
submission sent, and records one
`list.CopyBufferRegion(&shadow.buffer, a, staging, a, (b - a) as u64)` per range —
313 ranges and 8.6–11.5 ms a frame on this machine, plus the hash passes that
`VICTORY_CARD_REHASH_RUNS=1` folds together (96 passes over 102 pieces) and the
compare passes (357). That cost is command recording, not bandwidth: `card_dirty`
itself is a few take/upgrade/take calls.

### Getting the bytes back

The other direction is `shadow.rs:1952 back_piece(...)`, which uses
`first_difference`/`last_difference` (`shadow.rs:1369`, 32-byte chunks with a
scalar tail) to find the changed run inside a piece and reads back only that.
21–22 MB and 9–19 ms a frame, spread over threads by `wide_back()`
(`shadow.rs:1498`, on unless `VICTORY_WIDE_BACK=0`). Whole pieces are dropped
before that when the shader's writes are write-combined and
`VICTORY_WC_NO_BACK=1`: 8–26 GB a frame stay on the card instead of crossing back.

### What it actually costs here

| | per frame |
|---|---|
| `copy_mapped` bytes | 62–93 MB |
| `copy_mapped` time | 15–40 ms (≈4.1 GB/s) |
| fills | 7,000–12,000 × ~500 B, 5.3–7.8 ms on the submitting thread |
| card-side recording | 6.6–13.6 ms, 313 ranges |
| copy-back | 21–22 MB, 9–19 ms, beside the submitting thread |
| refresh filtering | 15.4–19.9 ms, of which 6,100–8,900 ranges known to copy nothing |

The wall is the write side: the CPU writes write-combined memory at about
**4.1 GB/s** while the card pulls the same bytes out of a shared buffer at
**19.4 GB/s** (`VICTORY_INPLACE_PROBE=1`: 64 MB in 3.3 ms). It is not a
thread-count wall — splitting one copy into 1 MiB pieces across the pool measured
4.06 GB/s against 4.09 GB/s serial, which is why that patch is not in this tree.

### What was tried and did not work

- **Wider copies** (1 MiB pieces over `wide_each` in `Step::Copy`):
  4.06 GB/s vs 4.09 GB/s serial. Reverted.
- **`FEW` 256 → 8** in `flush_all` (push the "known to copy nothing" filter onto
  the workers): refreshes went from 18.1 to 25.8–38.5 ms and `one at a time …
  before shader writes` from 0.7 to 4.3–4.9 ms. Reverted; the reason is in a
  comment at that constant.
- **`VICTORY_JOIN_COPIES=1`**: 0 of 10 copies continue the one before, so
  joining has nothing to join.
- **`VICTORY_CARD_STEPS=2`** (let the card do the copies): the log counts
  `0.00 fills, 0.00 buffer copies` because `card_steps` only picks up steps that
  are blocked behind an in-flight write (`card_steps.rs:286 plan`, and it is
  bounded by `const BETWEEN: u64 = 4 << 20`), so it swallows waits rather than
  moving bulk bytes.
- **Reading title memory in place instead of copying** (`VICTORY_INPLACE=2`) and
  **a CPU-mapped VRAM copy** (`VICTORY_WC_BAR=1`): both lose here, numbers in
  `RESEARCH-cpu-copies.md` and above — random PCIe reads took the GPU to 341–366
  ms a frame, and the driver reports `GPUUploadHeapSupported=false` and refuses a
  hand-built L1 heap with `E_INVALIDARG`.

Still open, not measured: putting `victory_safe_changed` in front of the fills so
a fill whose bytes did not change is not written at all. It is the only idea left
that removes bytes rather than rearranging them; the guess is 30–50% of the 66 MB,
and it is a guess.

Switches that matter, in one place: `VICTORY_WIDE_FILLS` (default on),
`VICTORY_PREPARE_WIDE` (off; wide from 32 fills), `VICTORY_REFRESH_ONCE`,
`VICTORY_LIGHT_REFRESH`, `VICTORY_WIDE_BACK` (on), `VICTORY_CARD_REHASH_RUNS`,
`VICTORY_WC_NO_BACK`, `VICTORY_JOIN_COPIES`, `VICTORY_JOIN_PARTS`,
`VICTORY_COPY_ASK` (a `VirtualQuery` before each copy; off — it cost ~2 ms a copy
in the title's big pools), `VICTORY_SYNC_FINE`, `VICTORY_SPANS_QUICK`.

## Shaders: choosing a form per program (the black water)

A console wave64 compute program has several forms on a 32-lane host: deep skips
(`without skips, scalars followed`), local waves, loose LDS, the paired form, and cool_fox's
wave-wide. Which one wins used to depend only on whether the translation went through, not
on which program it was — so a form that runs fine could still be the wrong answer for one
program.

Open water is the clearest case: the title's two 64-lane water programs

```
5dc3b25986622949   6f225b641c5b0708
```

come out as black and grey patches once the wave-wide form takes them. Left out of that
form, open water shows waves and the sun's path (`naixue2233/Victory-naixue2233`'s README,
2026-11-27). Upstream issue #62 is the same shape of bug for 14 `CS_ShadowRenderTiled*`
programs and the deep-skips form.

Four switches choose a form per program. Their value is the `hash=` column (16 hex digits,
comma or space separated):

| Switch | Meaning |
|---|---|
| `VICTORY_WAVE_WIDE_NOT` | these programs may not take the wave-wide form |
| `VICTORY_WAVE_WIDE_ONLY` | only these programs may |
| `VICTORY_WAVE_PAIRS_NOT` | these programs may not take the paired form |
| `VICTORY_WAVE_PAIRS_ONLY` | only these programs may |

`with_program(hash, f)` in `crates/victory-shader/src/emit/mod.rs` puts the program's hash in
a thread-local; `wave_wide_here()` and `pairs_here()` check the lists in front of every form
gate. The GPU side passes the same hash in two places — `crates/victory-gpu/src/pipeline.rs`
(the run's translation) and `crates/victory-gpu/src/shader_cache.rs` (`victory shaders`
warming) — and they have to agree, or a warmed translation would be the unfiltered one the
run then reuses. `crates/victory-shader/build.rs` scans the sources for the switch names, so
they join the shader cache key automatically: adding a switch needs no cache version bump.

`scripts/play-3070.ps1` carries the two water exclusions by default (`-NoWaterFix` runs the
stock choice instead; it is the same as
`-Extra "VICTORY_WAVE_WIDE_NOT=5dc3b25986622949,6f225b641c5b0708"`). To see what each program
tried, set `VICTORY_WAVE_WHY=1`: stderr gets `WAVE64-RETRY <hash> <form>: wave ...`, and a
program a list keeps out gets a `WAVE-WIDE-NOT <hash>` line.

## Build

`D3D12Core.dll` 1.620.2 ships in this repository at
`sdk/agility/build/native/bin/x64/D3D12Core.dll` (4,874,024 bytes, Microsoft's
Agility runtime redistributable — see `sdk/README.md`), so a clone builds
straight away:

```powershell
cargo build --release --bin victory    # target/release/victory.exe
```

`scripts/fetch-d3d12-sdk.ps1` is only for re-fetching that file or trying a
different runtime version (`-Version 1.619.6`). Whatever version it is, it has to
match `SDK_VERSION` in `apps/victory/src/sdk.rs` (620).

Single file, no VC++ redistributable (the core is embedded and written out at
runtime):

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --bin victory --target-dir target-static
```

If that file is missing the build still succeeds and the executable uses the
system runtime — which is exactly the case it cannot render with on Windows 10,
so keep the one in `sdk/`.

## Run

```powershell
./scripts/play-3070.ps1 -Game "D:\path\to\game_scarlett_master.exe" -Relay 2 -LandThreads 8
```

or, for the packaged build, `dist\Play-GTA6.cmd "D:\path\to\game_scarlett_master.exe"`.

First run translates and caches the title's shaders (~26,000, ~80 s, ~6 GB peak):
`victory.exe shaders <game dir>` then `victory.exe warm`.

## Numbers on this machine

RTX 3080 10 GB, 24 threads, Windows 10 19045, driver 617.42, 1920x1080:

| scene | fps | ms/frame | preparing (CPU) | copies | copy back | on the GPU |
|---|---|---|---|---|---|---|
| light | 20–22 | 45–51 | 31–35 | 16–18 ms / 62 MB | 10 ms / 21 MB | 4–13 |
| heavy | 12–17 | 58–83 | 37–39 | 19–20 ms / 65–70 MB | 9–11 ms / 22 MB | 13–25 |

The submitting thread is the wall, and inside it the title's bytes are the bulk:
the title writes its own write-combined memory and this renderer has to copy
those bytes into upload memory from the CPU, which runs at **4.1 GB/s** (measured;
the card reading them over DMA does 19.4 GB/s). Everything CPU-side outside those
copies adds up to ~34 ms/frame, so zeroing all of it would reach ~48 ms = 20.8 fps.

Two structural attempts measured and rejected:

* `VICTORY_INPLACE=2` (place the title's own memory as the card's heap: no copy,
  no refresh, no copy back) — copies do go to zero, but the GPU's random reads
  over PCIe cost 341–366 ms/frame → 2.4 fps. It also has to run with the relay
  and late wait off (write watching does not see GPU writes there).
* `VICTORY_WC_BAR` (the CPU writes a buffer in card memory directly) — the
  runtime reports `GPUUploadHeapSupported = false` on Windows 10 19045 with
  driver 617.42, and building the heap as `D3D12_HEAP_TYPE_CUSTOM` +
  `WRITE_COMBINE` + `MEMORY_POOL_L1` is refused with `0x80070057` (3385 attempts).

Also not to port: [fxceless/victory-fork#1424](https://github.com/fxceless/victory-fork/pull/1424)
(parallel copies) is closed unmerged; its own A/B was 6.7 → 5.7 fps, and the
4 MiB threshold never fires on real submissions (copies are 0.8–2.5 MB).

`RESEARCH-cpu-copies.md` records the mechanism inventory, the probe output, and
what each switch actually does.

## Licence

Upstream's `LICENSE` (GPL-3.0-or-later) applies. The D3D12 Agility runtime
embedded in the executable is Microsoft's redistributable
(`Microsoft.Direct3D.D3D12` 1.620.2, x64: `sdk/agility/build/native/bin/x64/D3D12Core.dll`,
Microsoft's own terms, not GPL) and it is kept in this repository only so a clone
builds without a network round trip.
