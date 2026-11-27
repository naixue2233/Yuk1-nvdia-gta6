//! Force the two D3D12 Agility SDK exports into the executable's export table,
//! and embed the Agility runtime in the binary so one file installs and runs.
//!
//! `#[no_mangle]` statics keep their symbol names, but a `bin` target does not
//! export symbols on its own, so the in-box `d3d12.dll` loader would not find
//! `D3D12SDKVersion` / `D3D12SDKPath`. `/EXPORT:<name>,DATA` on the MSVC link
//! adds the export table entries for the statics in `src/sdk.rs`.
//!
//! The core is copied into `OUT_DIR` for `include_bytes!` in `src/sdk.rs`, which
//! writes it out beside the executable before the first D3D12 call (the loader
//! only ever loads `D3D12Core.dll` from disk — it cannot be loaded from memory).
//! The copy is optional: without `sdk/agility/.../x64/D3D12Core.dll` the crate
//! still builds and falls back to the in-box runtime.

use std::path::{Path, PathBuf};

/// The vendored `D3D12Core.dll` of Microsoft.Direct3D.D3D12 1.620.2, x64.
const CORE: &str = "../../sdk/agility/build/native/bin/x64/D3D12Core.dll";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/sdk.rs");
    println!("cargo::rustc-check-cfg=cfg(bundled_core)");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
    println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");

    let core = Path::new(CORE);
    println!("cargo:rerun-if-changed={}", core.display());
    let Ok(out_dir) = std::env::var_os("OUT_DIR").ok_or(()).map(PathBuf::from) else {
        return;
    };
    if !core.is_file() {
        println!("cargo:warning=no {CORE}: the executable will not carry the Agility runtime");
        return;
    }
    let copy = out_dir.join("D3D12Core.dll");
    match std::fs::copy(core, &copy) {
        Ok(n) => {
            println!("cargo:rustc-cfg=bundled_core");
            println!("cargo:warning=embedding {n} bytes of D3D12Core.dll");
        }
        Err(e) => println!("cargo:warning=cannot copy {CORE}: {e}"),
    }
}
