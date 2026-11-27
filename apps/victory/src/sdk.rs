//! D3D12 Agility SDK bootstrap: carry the runtime in the executable and put it
//! on disk before the first D3D12 call, or step aside for the in-box one.
//!
//! The translated shaders use Shader Model 6.6 heap direct indexing
//! (`ResourceDescriptorHeap` / `SamplerDescriptorHeap`), and the host root
//! signatures carry `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` and
//! `SAMPLER_HEAP_DIRECTLY_INDEXED` (`pipeline/host.rs`, `victory-shader`'s
//! `emit/host.rs`). Those flags and the SM 6.6 DXIL that uses them exist in
//! the D3D12 Agility runtime, not in the in-box one: a stock Windows 10
//! `d3d12.dll` (10.0.19041.x) serialises them as
//!
//! ```text
//! root signature: Unsupported bit-flag set (root signature flags c00).
//! ```
//!
//! and then refuses every root signature with `E_INVALIDARG`, which loses the
//! pipelines boot needs.
//!
//! The in-box loader picks its runtime from two exports on the *executable*,
//! which must have exactly the shapes the loader reads:
//!
//! * `D3D12SDKVersion`: a `UINT` holding the Agility SDK number.
//! * `D3D12SDKPath`: a **pointer** to a null-terminated ASCII path, relative
//!   to the executable, of the directory holding `D3D12Core.dll`. Exporting
//!   the characters instead of a pointer makes the loader treat the first
//!   eight string bytes as an address and the process dies in `DllMain` with
//!   `STATUS_DLL_INIT_FAILED` (0xC0000142).
//!
//! The loader resolves `D3D12Core.dll` with `LoadLibrary` on that path: the
//! runtime cannot be loaded out of memory, so a single-file build still has to
//! write one file. [`prepare`] does that (from `include_bytes!`, see
//! `build.rs`) into `<exe>\D3D12\`, or `%LOCALAPPDATA%\victory\d3d12\<n>\` if
//! the executable's own directory is read-only, or leaves the version at 0 so
//! the in-box runtime is used — on a build that predates the Agility SDK
//! (before 10.0.18362), with that folder unwritable, or with
//! `VICTORY_D3D12_SDK=0`.

use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};

/// `D3D12_SDK_VERSION` of the runtime carried in the executable
/// (Microsoft.Direct3D.D3D12 1.620.2).
pub const SDK_VERSION: u32 = 620;

/// Folder beside the executable that [`prepare`] writes the core into.
const SDK_DIR: &str = "D3D12";

/// File name the loader appends to the path.
const CORE: &str = "D3D12Core.dll";

/// Directory holding `D3D12Core.dll`, relative to the executable and
/// null-terminated; the loader appends the file name itself.
static SDK_PATH: &[u8; 9] = b".\\D3D12\\\0";

/// The core, when the build found one to embed (`build.rs`).
#[cfg(bundled_core)]
static CORE_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/D3D12Core.dll"));
#[cfg(not(bundled_core))]
static CORE_BYTES: &[u8] = &[];

/// A `UINT` the loader reads through `GetProcAddress`, written before the
/// first D3D12 call and read-only afterwards; the cell is what makes writing
/// it at all possible behind a `static`.
#[repr(transparent)]
pub struct VersionSlot(UnsafeCell<u32>);

// SAFETY: written once, in `prepare`, before any D3D12 call and before any
// other thread exists; read-only from then on.
unsafe impl Sync for VersionSlot {}

impl VersionSlot {
    fn set(&self, version: u32) {
        // SAFETY: see the `Sync` note: no other thread exists yet.
        unsafe { *self.0.get() = version };
    }

    /// What the loader will read.
    pub fn get(&self) -> u32 {
        // SAFETY: a `u32` read of a cell only ever written before threads start.
        unsafe { *self.0.get() }
    }
}

/// A pointer to the path string, not the string. `*const u8` is not `Sync`,
/// and an exported static must be; the pointee is an immutable byte string
/// (either the `'static` default or a leaked one from [`prepare`]).
#[repr(transparent)]
pub struct PathSlot(UnsafeCell<*const u8>);

// SAFETY: see [`PathSlot`]: the pointer is replaced once in `prepare`, before
// any other thread exists, and the pointee outlives the process.
unsafe impl Sync for PathSlot {}

impl PathSlot {
    fn set(&self, path: &'static [u8]) {
        // SAFETY: see the `Sync` note.
        unsafe { *self.0.get() = path.as_ptr() };
    }
}

/// Read by the in-box `d3d12.dll` loader through `GetProcAddress` on the
/// executable's exports (see `build.rs`, which forces the export table
/// entries).
#[used]
#[unsafe(no_mangle)]
pub static D3D12SDKVersion: VersionSlot = VersionSlot(UnsafeCell::new(SDK_VERSION));

/// See [`D3D12SDKVersion`]: a pointer to the path string, not the string.
#[used]
#[unsafe(no_mangle)]
pub static D3D12SDKPath: PathSlot = PathSlot(UnsafeCell::new(SDK_PATH.as_ptr()));

/// One-line description for the `victory config` banner.
pub fn description() -> String {
    if D3D12SDKVersion.get() == 0 {
        return "D3D12 in-box runtime (no Agility core in use)".to_string();
    }
    format!("D3D12 Agility SDK {} (D3D12Core.dll beside the executable)", D3D12SDKVersion.get())
}

/// Puts the carried runtime where the loader can find it, or leaves the
/// version at 0 for the in-box one. Must run before the first D3D12 call
/// (`D3D12CreateDevice`, `D3D12SerializeVersionedRootSignature`,
/// `D3D12GetDebugInterface`) — `main` calls it first.
pub fn prepare() {
    if std::env::var_os("VICTORY_D3D12_SDK").is_some_and(|v| v == "0" || v == "off" || v == "system") {
        D3D12SDKVersion.set(0);
        eprintln!("victory: D3D12: the in-box runtime (VICTORY_D3D12_SDK=0)");
        return;
    }
    match windows_build() {
        Some(build) if build < 18_362 => {
            D3D12SDKVersion.set(0);
            eprintln!("victory: D3D12: Windows build {build} predates the Agility SDK: the in-box runtime");
            return;
        }
        None => {
            D3D12SDKVersion.set(0);
            eprintln!("victory: D3D12: cannot read the Windows build: the in-box runtime");
            return;
        }
        Some(_) => {}
    }
    match materialise() {
        Ok(path) => eprintln!("victory: D3D12 Agility SDK {} at {}", D3D12SDKVersion.get(), path.display()),
        Err(e) => {
            D3D12SDKVersion.set(0);
            eprintln!("victory: D3D12: no Agility core to put in place ({e}): the in-box runtime");
        }
    }
}

/// Writes the carried core where the loader will look and points
/// `D3D12SDKPath` at it. Returns the file written.
fn materialise() -> Result<PathBuf, String> {
    if CORE_BYTES.is_empty() {
        return Err("this build carries no D3D12Core.dll".to_string());
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("the executable has no directory")?.to_path_buf();

    let beside = dir.join(SDK_DIR).join(CORE);
    if up_to_date(&beside) {
        return Ok(beside);
    }
    match write_core(&dir.join(SDK_DIR)) {
        Ok(path) => Ok(path),
        Err(beside_err) => {
            // A read-only install (Program Files, a lockfile-protected tree):
            // the same one file goes under %LOCALAPPDATA%, and the path the
            // loader gets is the relative one to it.
            let local = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .ok_or_else(|| format!("{beside_err}; and no LOCALAPPDATA"))?;
            let alt = local.join("victory").join("d3d12").join(SDK_VERSION.to_string());
            let path = write_core(&alt).map_err(|e| format!("{beside_err}; and {e}"))?;
            let rel = relative(&dir, &alt).ok_or_else(|| format!("{beside_err}; and no relative path to {}", alt.display()))?;
            // The loader wants a null-terminated path relative to the
            // executable and keeps the pointer: leak it.
            let mut bytes = rel.into_bytes();
            bytes.push(0);
            let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            D3D12SDKPath.set(leaked);
            Ok(path)
        }
    }
}

/// Whether the core already beside the executable is the one carried here.
fn up_to_date(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.len() as usize == CORE_BYTES.len(),
        Err(_) => false,
    }
}

/// Writes the carried core into `dir` through a temporary name, so a reader
/// never sees half a DLL.
fn write_core(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(CORE);
    let tmp = dir.join(format!("{CORE}.{}", std::process::id()));
    std::fs::write(&tmp, CORE_BYTES).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(path)
}

/// `to` as a path relative to `base`, with backslashes, no trailing one.
fn relative(base: &Path, to: &Path) -> Option<String> {
    let base: Vec<_> = base.components().collect();
    let to: Vec<_> = to.components().collect();
    if base.first() != to.first() {
        return None;
    }
    let mut i = 0;
    while i < base.len() && i < to.len() && base[i] == to[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..base.len() {
        parts.push("..".to_string());
    }
    for c in &to[i..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    (!parts.is_empty()).then(|| parts.join("\\"))
}

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
}

/// The real build number (not the manifest-lied one `GetVersionEx` returns).
fn windows_build() -> Option<u32> {
    let mut v = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: the structure is `size`-initialised and lives for the call.
    let status = unsafe { RtlGetVersion(&mut v) };
    (status == 0 && v.major >= 10).then_some(v.build)
}
