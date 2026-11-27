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
6. **`dist/`** — the single-file distribution: `victory.exe` with the runtime
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
