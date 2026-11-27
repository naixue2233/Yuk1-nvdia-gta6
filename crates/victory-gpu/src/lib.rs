//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod compose;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
mod copystep; // (coder W) VICTORY_REFRESH_ACCOUNT=1 (per-copy refresh account)
#[cfg(all(windows, feature = "diagnostics"))]
mod faultbook;
#[cfg(windows)]
mod cores;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
pub mod fence;
pub mod frame;
pub mod chunks; // (fox port A) F21 arena chunk meter (VICTORY_CHUNK_METER)
#[cfg(windows)] pub mod framelog; // (fox port A) frame log, VICTORY_FRAME_LOG
pub mod hangwatch; // (fox port A) VICTORY_HANG_WATCH_S
pub mod lockmeter; // (coder H) F23 engine buffer lock meter (VICTORY_LOCK_METER)
pub mod knownwrites; // (coder J) own dirty ledger for buffer copies (VICTORY_KNOWN_WRITES=verify|1)
pub mod hlehooks; // (fox port A) runtime notes with no fox GPU function yet (title_memory_unmapped)
#[cfg(windows)] pub mod shell; // (fox port A) caption text and the game's logo icons
#[cfg(all(windows, feature = "diagnostics"))]
mod book;
#[cfg(all(windows, feature = "diagnostics"))]
mod holders; // (fox F) VICTORY_HOLDERS: which buffer of ours lies where in the PC's memory
#[cfg(windows)]
mod heap_marks;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod port;
#[cfg(windows)]
mod land;
pub mod list;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod picked;
#[cfg(windows)]
mod pool;
#[cfg(all(windows, feature = "diagnostics"))]
mod popwatch;
#[cfg(windows)]
mod primitive;
pub mod query;
pub mod queue;
#[cfg(all(windows, feature = "diagnostics"))]
pub mod replay;
#[cfg(windows)]
mod residency;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod safecopy;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shadow;
// (Fox T) VICTORY_REFRESH_STALE=verify: the stale-page refresh's verify harness (W's hook calls it)
mod stalecheck;
// (Fox T) VICTORY_NAN_METER=1: NaNs and infinities counted per program where they are stored
#[cfg(feature = "diagnostics")]
mod nanmeter;
#[cfg(windows)]
mod tiles;
pub mod signature;
mod switch;
#[cfg(all(windows, feature = "diagnostics"))]
mod ab;
#[cfg(windows)]
mod tdr;
#[cfg(windows)]
mod texture;
pub mod timeline;
#[cfg(windows)]
pub mod titlewrite;
#[cfg(windows)]
mod upload;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
pub mod loadmeter; // (coder G) VICTORY_LOAD_READ_METER
#[cfg(windows)]
pub mod pagekinds; // (coder G) VICTORY_PAGE_KINDS (F24/F25)
#[cfg(windows)]
mod texback;
// (Fox port C2) F11 texture detail meter (VICTORY_TEXTURE_DETAIL, off by default).
#[cfg(all(windows, feature = "diagnostics"))]
mod texdetail;

#[cfg(windows)]
pub use host::on_failure;

/// Whether `switch` is on now, if it is one of those `VICTORY_AB` turns on and
/// off while the title runs (`ab`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    ab::pick(switch)
}

/// What the frame is at (`copystat::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    copystat::phase()
}
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view};
#[cfg(windows)]
pub use shadow::title_memory_write_combined;
#[cfg(windows)]
pub use watch::{
    kernel_writing, title_memory_allocated, title_memory_freed, title_memory_reading, title_memory_remapped,
    Remapping, title_memory_remapping, title_memory_remapping_in, remap_notes_take, title_memory_splitting, title_memory_unmapping, title_memory_written,
};
// (V2, `VICTORY_SOFT_VIEWS`) Views of the page-mapping copies kept in the runtime's books alone.
#[cfg(windows)]
pub use watch::{
    SOFT_BY_OUR_FAULT, SOFT_BY_OUR_WRITE, SOFT_BY_TITLE_CALL, SOFT_BY_TITLE_FAULT, SoftViews, soft_make_real, soft_view_counts,
    soft_view_gone, soft_view_noted, soft_view_real, soft_view_would, use_soft_views,
};
#[cfg(windows)]
pub use lockmeter::kernel_will_write; // (coder H) watch::writing, noted for the F23 meter first
