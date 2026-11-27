sdk/
====

The D3D12 Agility runtime this build embeds.

```
agility/build/native/bin/x64/D3D12Core.dll
  4,874,024 bytes, SHA256 70A6D47D335B4A15180544C01A35B84474522A369DC582D85FE057313A3AEAA0
  FileVersion 1.620.2.0.20261106.5
```

`apps/victory/build.rs` copies it into `OUT_DIR` and `include_bytes!`s it, so the
executable carries the runtime and `src/sdk.rs` writes it out on the first run.
`D3D12_CORE` in that file is the path it looks for.

Where it comes from: the `Microsoft.Direct3D.D3D12` 1.620.2 NuGet package, x64
runtime redistributable, © Microsoft Corporation (Agility SDK redistribution is
covered by Microsoft's own terms, not by this repository's licence). Only the x64
`D3D12Core.dll` is kept — not the NuGet package, its headers, or the other
architectures.

```powershell
./scripts/fetch-d3d12-sdk.ps1                 # 1.620.2, the same file
./scripts/fetch-d3d12-sdk.ps1 -Version 1.619.6
./scripts/fetch-d3d12-sdk.ps1 -Proxy 127.0.0.1:7897
```

Why it is here rather than downloaded at build time: without it the executable
falls back to the in-box `d3d12.dll`, which on Windows 10 (10.0.19041) has no
Shader Model 6.6 heap direct indexing — it answers the root signatures this
renderer emits with `root signature: Unsupported bit-flag set (root signature
flags c00)` and `E_INVALIDARG`, and boot loses every pipeline. The embedded copy
also has to match the `D3D12SDKVersion` exported by `apps/victory/src/sdk.rs`
(620): the loader asks for `D3D12Core.dll` in the folder `D3D12SDKPath` names and
checks that its version matches.
