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
