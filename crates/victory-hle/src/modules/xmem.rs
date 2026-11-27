//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

/// Bytes of physical pages the title holds (`XMemAllocatePhysicalPages`
/// less `XMemFreePhysicalPages`): its own memory, as a console counts it.
static TITLE_PAGES: AtomicUsize = AtomicUsize::new(0);

fn title_used_is_process() -> bool {
    static PROCESS: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("VICTORY_TITLE_USED").is_ok_and(|v| v.trim() == "process"));
    *PROCESS
}

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    // (V2, `VICTORY_SOFT_VIEWS`) The host acts on what it has mapped there.
    if !base.is_null() {
        soft::title_asks(base as usize, size);
    }
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let asked = protect;
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if at.is_null() {
        let error = std::io::Error::last_os_error();
        if kind & MEM_COMMIT != 0 {
            commit_refused(size, "virtual memory", error);
        } else {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
        }
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    if !at.is_null() && !placeholder {
        victory_gpu::title_memory_allocated(at as usize, size);
    }
    // Write-combined: memory the title's CPU writes and does not read.
    if !at.is_null() && asked & 0x400 != 0 {
        victory_gpu::title_memory_write_combined(at as usize, size);
    }
    if !at.is_null() && constrained && kind & MEM_COMMIT == 0 && flags & ADDRESS_32BIT != 0 {
        LOW_RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner()).push((at as usize, size));
    }
    note_protection(asked, kind, flags, at as usize, size);
    at
}

/// What the title asks its memory to be (the console's protection, with the
/// bits that say how each processor reaches it: 0x200 uncached, 0x400
/// write-combined, 0x800 and on the GPU's): said for the 1st, 2nd, 4th.. time of
/// each, with how much so far. (For placing it: memory the CPU reaches
/// uncached or write-combined is memory it does not read.)
fn note_protection(protect: u32, kind: u32, flags: u64, at: usize, size: usize) {
    type Seen = std::collections::HashMap<(u32, u32, u64), (u64, usize)>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let (n, bytes) = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let e = seen.get_or_insert_with(Default::default).entry((protect, kind, flags)).or_default();
        *e = (e.0 + 1, e.1 + size);
        *e
    };
    if n.is_power_of_two() {
        tracing::info!(
            "title memory asked for as protection {protect:#x}, kind {kind:#x}, flags {flags:#x}: {n} times, {} MB, last {at:#x}+{size:#x}",
            bytes >> 20
        );
    }
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // (F10, `VICTORY_MAP_DEFER`) What the title asked for so far, as it asked.
    DEFER.before(address as usize, 1);
    // (V2, `VICTORY_SOFT_VIEWS`) The host answers of what it has mapped.
    soft::title_asks(address as usize, 1);
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
        // (coder G) The load-read meter's guard bit is not the title's.
        if victory_gpu::loadmeter::on() {
            info.host.Protect &= !0x100;
        }
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section {
    handle: HANDLE,
    pages: usize,
    /// A view of the whole section, kept ([`Section::window`]); 0 if the
    /// host gave none.
    window: std::sync::OnceLock<usize>,
    /// A bit per page: committed, as far as [`Section::window`] asked.
    committed: Box<[std::sync::atomic::AtomicU64]>,
    /// (`VICTORY_POOL_DISCARD`) A bit per page: written through
    /// [`Section::window`] and not discarded since (resident in our view).
    resident: Box<[std::sync::atomic::AtomicU64]>,
    /// (`VICTORY_POOL_DISCARD`) A bit per page: discarded and not written since.
    discarded: Box<[std::sync::atomic::AtomicU64]>,
}

impl Section {
    /// Where bytes of `n` pages from page `index` on can be written,
    /// whether or not the pages are mapped anywhere, committed.
    ///
    /// Every DirectStorage write mapped a view of its pages, committed it,
    /// faulted each of its 4 KiB pages in and unmapped it again, all of it
    /// under the process's one address-space lock, with 24 workers, the
    /// title's own mapping calls (about 100,000 views of one page) and our
    /// guards asking for the same lock. One view of the section, kept,
    /// costs that once per page.
    /// `VICTORY_SECTION_WINDOW=0`: a view for each write, as before.
    fn window(&self, index: usize, n: usize) -> Option<usize> {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        static ON: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("VICTORY_SECTION_WINDOW").is_none_or(|v| v != "0"));
        if !*ON || index + n > self.pages {
            return None;
        }
        let base = *self.window.get_or_init(|| {
            // SAFETY: the whole of a section we own.
            unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, self.pages * PAGE) }.Value as usize
        });
        if base == 0 {
            return None;
        }
        let missing = |page: usize| self.committed[page / 64].load(Ordering::Acquire) >> (page % 64) & 1 == 0;
        let mut page = index;
        while page < index + n {
            if !missing(page) {
                page += 1;
                continue;
            }
            let run = (page..index + n).take_while(|&p| missing(p)).count();
            if run == 0 {
                continue; // another worker committed it meanwhile
            }
            if !commit(base + page * PAGE, run * PAGE) {
                return None;
            }
            for p in page..page + run {
                self.committed[p / 64].fetch_or(1 << (p % 64), Ordering::AcqRel);
            }
            page += run;
        }
        pool_discard::written(self, index, n);
        Some(base + index * PAGE)
    }
}

impl Section {
    /// (V2, `VICTORY_SOFT_VIEWS`) The section's lasting view ([`Section::window`]),
    /// made if it is not yet, without committing anything; `None` without one.
    fn lasting(&self) -> Option<usize> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        static ON: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("VICTORY_SECTION_WINDOW").is_none_or(|v| v != "0"));
        if !*ON {
            return None;
        }
        let base = *self.window.get_or_init(|| {
            // SAFETY: the whole of a section we own.
            unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, self.pages * PAGE) }.Value as usize
        });
        (base != 0).then_some(base)
    }

    /// (V2) Whether page `index` is committed (as far as a view asked).
    fn page_committed(&self, index: usize) -> bool {
        on_wine()
            || self.committed.get(index / 64).is_some_and(|w| w.load(std::sync::atomic::Ordering::Acquire) >> (index % 64) & 1 != 0)
    }

    /// Commits the pages from `index` on that a view at `base` shows, unless
    /// all of them are ([`lean`]): committed for one view is for all.
    fn commit_shown(&self, index: usize, base: usize, len: usize) -> bool {
        use std::sync::atomic::Ordering;
        let pages = index..(index + len / PAGE).min(self.pages);
        let bit = |p: usize| (&self.committed[p / 64], 1 << (p % 64));
        let done = || {
            pages.clone().all(|p| {
                let (word, bit) = bit(p);
                word.load(Ordering::Acquire) & bit != 0
            })
        };
        if lean() && done() {
            return true;
        }
        if !counted(Call::Commit, || commit(base, len)) {
            return false;
        }
        for p in pages {
            let (word, bit) = bit(p);
            word.fetch_or(bit, Ordering::AcqRel);
        }
        true
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        if let Some(&base) = self.window.get()
            && base != 0
        {
            // SAFETY: our view; whoever writes through it holds the section.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: base as *mut c_void,
                })
            };
        }
        victory_gpu::title_memory_section_gone(self.handle as usize);
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Placeholders that are some view's own, left by its unmapping: address
    /// and length. The title may have joined one with others since.
    holes: HashMap<usize, usize>,
}

impl Physical {
    /// What the page at `at` shows.
    fn slice_at(&self, at: usize) -> Option<Slice> {
        self.views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
    }
}

/// The title's pages by number.
struct Pages {
    next_page: usize,
    pages: HashMap<usize, Slice>,
}

/// Where the title sees its pages: [`Physical::views`] the other way.
/// Locked apart from them, and only for a moment ([`PAGES`]).
#[derive(Default)]
struct Shown {
    /// The addresses that show a page, by the page's section and index.
    /// Views of more than [`SHORT`] pages are in `long`.
    pages: HashMap<(usize, usize), Vec<usize>>,
    /// Address, bytes, section, first page.
    long: Vec<(usize, usize, usize, usize)>,
}

/// Pages of a view noted page by page in [`Shown::pages`].
const SHORT: usize = 64;

/// Taken after [`PHYSICAL`] where both are.
static SHOWN: LazyLock<RwLock<Shown>> = LazyLock::new(Default::default);

fn shown_mut() -> RwLockWriteGuard<'static, Shown> {
    SHOWN.write().unwrap_or_else(|e| e.into_inner())
}

impl Shown {
    fn note(&mut self, base: usize, len: usize, slice: &Slice) {
        let section = Arc::as_ptr(&slice.section) as usize;
        super::pagekinds::shown(base, len, section, slice.index);
        if len / PAGE > SHORT {
            self.long.push((base, len, section, slice.index));
            return;
        }
        for k in 0..len / PAGE {
            self.pages.entry((section, slice.index + k)).or_default().push(base + k * PAGE);
        }
    }

    fn forget(&mut self, base: usize, len: usize, slice: &Slice) {
        super::pagekinds::unshown(base, len);
        if len / PAGE > SHORT {
            self.long.retain(|view| view.0 != base);
            return;
        }
        let section = Arc::as_ptr(&slice.section) as usize;
        for k in 0..len / PAGE {
            let key = (section, slice.index + k);
            if let Some(at) = self.pages.get_mut(&key) {
                at.retain(|&at| at != base + k * PAGE);
                if at.is_empty() {
                    self.pages.remove(&key);
                }
            }
        }
    }

    /// Where the title sees the page at `slice`: addresses and bytes.
    fn at(&self, slice: &Slice) -> Vec<(usize, usize)> {
        let section = Arc::as_ptr(&slice.section) as usize;
        let mut out: Vec<(usize, usize)> =
            self.pages.get(&(section, slice.index)).into_iter().flatten().map(|&at| (at, PAGE)).collect();
        for &(base, len, of, index) in &self.long {
            if of == section && (index..index + len / PAGE).contains(&slice.index) {
                out.push((base + (slice.index - index) * PAGE, PAGE));
            }
        }
        out
    }
}

/// See [`pages_written`].
pub(crate) fn stream_notice() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_STREAM_NOTICE").is_none_or(|v| v != "0"));
    *ON
}

/// The views, changed by the title's mapping calls; held for as long as
/// those take (the host maps, commits and unmaps under it).
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(Default::default);

fn physical() -> MutexGuard<'static, Physical> {
    if !remap_meter::on() {
        return PHYSICAL.lock().unwrap_or_else(|e| e.into_inner());
    }
    // (V, remap meter) The wait for the page table, into the call's parts.
    let t = std::time::Instant::now();
    let held = PHYSICAL.lock().unwrap_or_else(|e| e.into_inner());
    remap_meter::part(remap_meter::P_TABLE_LOCK, t.elapsed().as_nanos() as u64);
    held
}

/// The title's pages. Apart from the views: DirectStorage looks pages up at
/// every request, from 24 workers, and used to wait there for the title's
/// mapping calls (12,000 waits of 1 to 170 ms in a load of 90 s). Written
/// only where the title allocates or frees pages. Taken after [`PHYSICAL`]
/// where both are.
static PAGES: LazyLock<RwLock<Pages>> = LazyLock::new(|| {
    RwLock::new(Pages {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        pages: HashMap::new(),
    })
});

fn page_table() -> RwLockReadGuard<'static, Pages> {
    PAGES.read().unwrap_or_else(|e| e.into_inner())
}

fn page_table_mut() -> RwLockWriteGuard<'static, Pages> {
    PAGES.write().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section {
        handle,
        pages: n,
        window: std::sync::OnceLock::new(),
        committed: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
        resident: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
        discarded: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
    });
    TITLE_PAGES.fetch_add(size, Ordering::Relaxed);
    let mut table = page_table_mut();
    let first = table.next_page;
    table.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        table.pages.insert(first + index * FRAMES, slice);
    }
    // Title memory is touched without our locks: a fault there may wait for
    // a mapping call, which takes them.
    drop(table);
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut table = page_table_mut();
    // Dropped without the lock: the last page of a section unmaps its window.
    let gone: Vec<Slice> = numbers.iter().filter_map(|number| table.pages.remove(number)).collect();
    drop(table);
    let freed = gone.len() * PAGE;
    let _ = TITLE_PAGES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| Some(held.saturating_sub(freed)));
    drop(gone);
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not mapped for a write");
            return None;
        }
        let view = View { at, len };
        if !commit(at.Value as usize, len) {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not committed for a write");
            return None;
        }
        Some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let table = page_table();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let Some(slice) = table.pages.get(page) else {
            tracing::warn!(page, offset, len, "a physical page the title does not own (any more)");
            return None;
        };
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// (coder G, `VICTORY_PAGE_KINDS`) The pool pages (section, index) under bytes `[offset, offset + len)` of
/// `pages` and the bytes of each; unknown pages left out.
pub(crate) fn page_keys(pages: &[usize], offset: usize, len: usize) -> Vec<((usize, usize), usize)> {
    let end = offset.saturating_add(len);
    let first = offset / PAGE;
    let Some(numbers) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let table = page_table();
    numbers
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let slice = table.pages.get(n)?;
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            Some(((Arc::as_ptr(&slice.section) as usize, slice.index), hi - lo))
        })
        .collect()
}

/// (coder G, `VICTORY_PAGE_KINDS`) Our whole-section views (the linear view DirectStorage writes through):
/// address and bytes of each section that has one.
/// (coder K, `VICTORY_GPU_ONLY=1` refusal log) Which view of ours holds `at`: a title mapping (our record of
/// the title's views), our own section window, or neither, with what Windows says of it.
pub(crate) fn describe(at: usize) -> String {
    let title = {
        let p = physical();
        p.views
            .range(..=at)
            .next_back()
            .filter(|(b, v)| at < *b + v.0)
            .map(|(b, v)| format!("title view {b:#x}+{:#x} (section {:#x} page {})", v.0, v.1.section.handle as usize, v.1.index))
    };
    let window = section_windows().into_iter().find(|&(a, n)| (a..a + n).contains(&at)).map(|(a, n)| format!("our section window {a:#x}+{n:#x}"));
    let query = {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data the query fills in.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            "no region".to_string()
        } else {
            format!("allocation {:#x} type {:#x} state {:#x} protect {:#x}", info.AllocationBase as usize, info.Type, info.State, info.Protect)
        }
    };
    format!("{}; {}; {query}", title.unwrap_or_else(|| "no title view".into()), window.unwrap_or_else(|| "no window of ours".into()))
}

/// (coder K, `VICTORY_GPU_ONLY=1`) The title's linear views of its pool: one view of a whole section from its
/// first page (run-144: 0xe1c800000 + 6,236 MB, every pool page): address, bytes, section handle.
pub(crate) fn linear_views() -> Vec<(usize, usize, usize)> {
    let p = physical();
    p.views
        .iter()
        .filter(|(_, (len, slice))| slice.index == 0 && *len == slice.section.pages * PAGE && *len >= 64 << 20)
        .map(|(&b, (len, slice))| (b, *len, slice.section.handle as usize))
        .collect()
}

pub(crate) fn section_windows() -> Vec<(usize, usize)> {
    let table = page_table();
    let mut seen = std::collections::BTreeMap::new();
    for slice in table.pages.values() {
        if let Some(&w) = slice.section.window.get()
            && w != 0
        {
            seen.insert(w, slice.section.pages * PAGE);
        }
    }
    seen.into_iter().collect()
}

/// Whether the title owns every page under bytes `[offset, offset + len)`.
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    let table = page_table();
    pages
        .get(offset / PAGE..(offset + len).div_ceil(PAGE))
        .is_some_and(|pages| pages.iter().all(|page| table.pages.contains_key(page)))
}

/// Where the title sees the pages under bytes `[offset, offset + len)` now,
/// in order (for the streaming trace: whether that changed while a request
/// wrote them).
pub(crate) fn pages_shown(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let numbers = pages.get(offset / PAGE..(offset + len).div_ceil(PAGE)).unwrap_or_default();
    let slices: Vec<Slice> = {
        let table = page_table();
        numbers.iter().filter_map(|n| table.pages.get(n)).cloned().collect()
    };
    let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
    let mut at: Vec<(usize, usize)> = slices.iter().flat_map(|slice| shown.at(slice)).collect();
    at.sort_unstable();
    at
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let k = (n * PAGE - within).min(data.len() - done);
        if let Some(at) = slice.section.window(slice.index, *n) {
            // SAFETY: committed bytes of the section's view, which `runs`
            // holds; the title's other uses of them race as on the console.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), (at + within) as *mut u8, k) };
        } else {
            let Some(mut view) = View::map(slice, *n) else {
                return false;
            };
            view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        }
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: Window,
    within: usize,
    len: usize,
}

enum Window {
    /// In the section's own view, which the section holds.
    Kept(usize, #[expect(dead_code, reason = "held for the view")] Arc<Section>),
    Own(View),
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        match &mut self.view {
            // SAFETY: committed bytes of the section's view, ours alone as
            // far as the runtime goes (other views alias them, as the
            // console's do).
            Window::Kept(at, _) => unsafe { std::slice::from_raw_parts_mut((*at + within) as *mut u8, len) },
            Window::Own(view) => &mut view.bytes()[within..within + len],
        }
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let view = match slice.section.window(slice.index, *n) {
        Some(at) => Window::Kept(at, slice.section.clone()),
        None => Window::Own(View::map(slice, *n)?),
    };
    Some(PageWindow {
        view,
        within: offset % PAGE,
        len,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it. Empty if no view shows them. Byte for byte, not by
/// whole pages: the title packs several small textures into one page and
/// fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        touched.iter().map(|page| table.pages.get(page).cloned()).collect()
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
        for (i, slice) in slices.iter().enumerate() {
            let Some(slice) = slice else { continue };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            // (F10, `VICTORY_MAP_DEFER`) Not where an unmapping is pending:
            // once applied, nothing shows the bytes there.
            pieces.extend(shown.at(slice).into_iter().filter(|(at, _)| !DEFER.pending_at(*at)).map(|(at, _)| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// DirectStorage has put bytes `[offset, offset + len)` of `pages` into
/// memory, through a view of our own: the GPU side is told where the title
/// sees them, so that what it copied there before they arrived (a texture
/// uploaded when its view was written holds whatever the pages held then)
/// is copied again. Call when the bytes are there.
/// `VICTORY_STREAM_NOTICE=0`: not told; what is then done is
/// `VICTORY_PAGE_WRITES`' to say (`victory_gpu::title_memory_written`).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    for (at, bytes) in shown_at(pages, offset, len) {
        if stream_notice() {
            victory_gpu::kernel_will_write(at, bytes);
        } else {
            victory_gpu::title_memory_written(at, bytes, true);
        }
    }
}

/// What the title's mapping calls cost since the last asking, for the
/// streaming log, counted only while that log is on: calls, pages,
/// milliseconds in all, the longest one's and waiting for the lock; the same
/// of the GPU's page-mapping copies ([`map_alias`]; calls and milliseconds);
/// and by system call (unmap, split, map, commit) how many and their
/// milliseconds.
static MAPPING: [std::sync::atomic::AtomicU64; 15] = [const { std::sync::atomic::AtomicU64::new(0) }; 15];

pub(crate) fn map_costs() -> Option<String> {
    let n: [u64; 15] = std::array::from_fn(|i| MAPPING[i].swap(0, Ordering::Relaxed));
    let time = |i: usize| matches!(i, 2 | 3 | 4 | 6 | 8 | 10 | 12 | 14);
    (n[0] + n[5] > 0).then(|| {
        let all: Vec<String> = n
            .iter()
            .enumerate()
            .map(|(i, &n)| if time(i) { n / 1_000_000 } else { n })
            .map(|n| n.to_string())
            .collect();
        all.join("\t")
    })
}

/// The system calls that change a mapping, as [`MAPPING`] counts them.
#[derive(Clone, Copy)]
enum Call {
    Unmap = 7,
    Split = 9,
    Map = 11,
    Commit = 13,
}

/// (C3, item 2c) `VICTORY_REMAP_METER=1`, default off: the title's mapping
/// calls and the system calls in them, a line every five seconds: per kind of
/// system call (unmap, split, map, commit) calls, µs and the longest; per
/// mapping call (CPU view, GPU view copies) calls, pages, µs, of that the
/// wait for our page table, and calls over 5 ms; and the first 50 system calls
/// over 5 ms (then every 100th) with the free RAM at that moment.
mod remap_meter {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub(super) fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("VICTORY_REMAP_METER").is_ok_and(|v| v.trim() == "1"))
    }

    /// Per system call kind (unmap, split, map, commit): calls, ns, max ns,
    /// over 5 ms.
    static SYS: [[AtomicU64; 4]; 4] = [const { [const { AtomicU64::new(0) }; 4] }; 4];
    /// Mapping calls: calls, pages, ns, wait ns, over 5 ms, max ns.
    static CALLS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
    static LONG_SAID: AtomicU64 = AtomicU64::new(0);
    /// The mapping lock waited for (ns), and its longest wait.
    static LOCK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

    /// (V) Parts of one mapping call, on its thread: system calls by kind
    /// (unmap, split, map, commit: count, ns), the page table's lock, the
    /// mapping lock (readers in place), our notes when the change is let go.
    pub(super) const P_TABLE_LOCK: usize = 4;
    pub(super) const P_MAP_LOCK: usize = 5;
    pub(super) const P_NOTES: usize = 6;
    const PARTS: usize = 7;

    thread_local! {
        static NOW: std::cell::RefCell<Option<[(u64, u64); PARTS]>> = const { std::cell::RefCell::new(None) };
    }

    /// Adds `ns` to part `kind` of the call this thread is in (if any).
    pub(super) fn part(kind: usize, ns: u64) {
        NOW.with(|n| {
            if let Some(p) = n.borrow_mut().as_mut() {
                p[kind].0 += 1;
                p[kind].1 += ns;
            }
        });
    }

    /// The system calls (unmap, split, map) of the call this thread is in.
    pub(super) fn sys_now() -> [u64; 3] {
        NOW.with(|n| n.borrow().as_ref().map_or([0; 3], |p| [p[0].0, p[1].0, p[2].0]))
    }

    /// Calls over [`LONG_MS`] logged so far.
    static LONG_LOGGED: AtomicU64 = AtomicU64::new(0);
    const LONG_MS: u64 = 50;

    fn thread_ns() -> (u64, u64) {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut a, mut b, mut k, mut u) = (z, z, z, z);
        // SAFETY: out pointers of ours, the current thread.
        if unsafe { GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u) } == 0 {
            return (0, 0);
        }
        let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) * 100;
        (t(k), t(u))
    }

    fn faults() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: plain data, its size given.
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: as above.
        if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0 { u64::from(c.PageFaultCount) } else { 0 }
    }

    /// One outermost mapping call's figures at its start.
    pub(super) struct Parts {
        what: &'static str,
        address: usize,
        len: usize,
        cpu: (u64, u64),
        faults: u64,
        /// (V) The overlap clock of our address-space calls at the start.
        overlap: u64,
    }

    impl Parts {
        /// `None` inside another call of this thread (its parts go to that one).
        pub(super) fn begin(what: &'static str, address: usize, len: usize) -> Option<Parts> {
            let outer = NOW.with(|n| {
                let mut n = n.borrow_mut();
                if n.is_some() {
                    return false;
                }
                *n = Some([(0, 0); PARTS]);
                true
            });
            outer.then(|| Parts { what, address, len, cpu: thread_ns(), faults: faults(), overlap: super::map_quiet::overlap_begin() })
        }

        /// The call took `ns` for `pages`: a line if it took over [`LONG_MS`].
        pub(super) fn end(self, ns: u64, pages: usize) {
            let parts = NOW.with(|n| n.borrow_mut().take()).unwrap_or([(0, 0); PARTS]);
            if ns < LONG_MS * 1_000_000 {
                return;
            }
            let n = LONG_LOGGED.fetch_add(1, Relaxed);
            if n >= 2000 && !n.is_multiple_of(100) {
                return;
            }
            let (k1, u1) = thread_ns();
            let faults = faults().saturating_sub(self.faults);
            let (overlap, in_flight, longest) = super::map_quiet::overlap_end(self.overlap);
            let ms = |v: u64| v as f64 / 1e6;
            let known: u64 = parts.iter().map(|p| p.1).sum();
            tracing::info!(
                call = self.what,
                address = format_args!("{:#x}", self.address),
                bytes = self.len,
                pages,
                ms = ms(ns),
                unmap = format_args!("{} calls {:.1} ms", parts[0].0, ms(parts[0].1)),
                split = format_args!("{} calls {:.1} ms", parts[1].0, ms(parts[1].1)),
                map = format_args!("{} calls {:.1} ms", parts[2].0, ms(parts[2].1)),
                commit = format_args!("{} calls {:.1} ms", parts[3].0, ms(parts[3].1)),
                page_table_lock = format_args!("{} waits {:.1} ms", parts[P_TABLE_LOCK].0, ms(parts[P_TABLE_LOCK].1)),
                mapping_lock = format_args!("{} waits {:.1} ms", parts[P_MAP_LOCK].0, ms(parts[P_MAP_LOCK].1)),
                our_notes = format_args!("{} lets go {:.1} ms", parts[P_NOTES].0, ms(parts[P_NOTES].1)),
                other_ms = ms(ns.saturating_sub(known)),
                thread_kernel_ms = ms(k1.saturating_sub(self.cpu.0)),
                thread_user_ms = ms(u1.saturating_sub(self.cpu.1)),
                process_faults = faults,
                ours_in_flight_elsewhere_ms = ms(overlap),
                ours_in_flight_most = in_flight,
                ours_longest_ms = ms(longest),
                free_mb = free_mb(),
                n = n + 1,
                "remap meter: a mapping call of the title's over {LONG_MS} ms, by part (other = in the call but in none of the parts; thread kernel+user below the call's time = the thread waited)"
            );
        }
    }

    pub(super) fn lock(ns: u64) {
        LOCK[0].fetch_add(ns, Relaxed);
        LOCK[1].fetch_max(ns, Relaxed);
    }
    static LINE_MS: AtomicU64 = AtomicU64::new(0);
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    fn free_mb() -> u64 {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data, its size set.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: a valid out pointer.
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 { m.ullAvailPhys >> 20 } else { 0 }
    }

    pub(super) fn sys(kind: usize, ns: u64) {
        let s = &SYS[kind.min(3)];
        s[0].fetch_add(1, Relaxed);
        s[1].fetch_add(ns, Relaxed);
        s[2].fetch_max(ns, Relaxed);
        if ns > 5_000_000 {
            s[3].fetch_add(1, Relaxed);
            let n = LONG_SAID.fetch_add(1, Relaxed);
            if n < 50 || n.is_multiple_of(100) {
                tracing::info!(
                    kind = ["unmap", "split", "map", "commit"][kind.min(3)],
                    ms = ns as f64 / 1e6,
                    free_mb = free_mb(),
                    n = n + 1,
                    "remap meter: a system call of a mapping over 5 ms"
                );
            }
        }
        line();
    }

    pub(super) fn call(pages: usize, ns: u64, waited: u64) {
        CALLS[0].fetch_add(1, Relaxed);
        CALLS[1].fetch_add(pages as u64, Relaxed);
        CALLS[2].fetch_add(ns, Relaxed);
        CALLS[3].fetch_add(waited, Relaxed);
        if ns > 5_000_000 {
            CALLS[4].fetch_add(1, Relaxed);
        }
        CALLS[5].fetch_max(ns, Relaxed);
    }

    pub(super) fn line() {
        let now = EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
        let last = LINE_MS.load(Relaxed);
        if now < last + 5000 || LINE_MS.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
            return;
        }
        let mut text = String::new();
        for (name, s) in ["unmap", "split", "map", "commit"].iter().zip(&SYS) {
            let n: Vec<u64> = s.iter().map(|a| a.swap(0, Relaxed)).collect();
            text.push_str(&format!(
                "; {name} {} calls {:.1} ms (mean {:.0} us, longest {:.1} ms, over 5 ms {})",
                n[0],
                n[1] as f64 / 1e6,
                if n[0] == 0 { 0.0 } else { n[1] as f64 / n[0] as f64 / 1e3 },
                n[2] as f64 / 1e6,
                n[3]
            ));
        }
        let c: Vec<u64> = CALLS.iter().map(|a| a.swap(0, Relaxed)).collect();
        let lock: Vec<u64> = LOCK.iter().map(|a| a.swap(0, Relaxed)).collect();
        text.push_str(&super::map_runs::line());
        text.push_str(&super::map_quiet::line());
        text.push_str(&format!(
            "; the mapping lock waited for {:.1} ms (longest {:.1} ms); our notes of the changes (guards, views, records) {:.1} ms",
            lock[0] as f64 / 1e6,
            lock[1] as f64 / 1e6,
            victory_gpu::remap_notes_take() as f64 / 1e6
        ));
        tracing::info!(
            "remap meter (VICTORY_REMAP_METER=1), last 5 s: mapping calls {} of {} pages, {:.1} ms (waiting for our page table {:.1} ms; \
             longest {:.1} ms, over 5 ms {}){text}; free RAM {} MB",
            c[0],
            c[1],
            c[2] as f64 / 1e6,
            c[3] as f64 / 1e6,
            c[5] as f64 / 1e6,
            c[4],
            free_mb()
        );
    }
}

/// [`victory_gpu::title_memory_remapping_in`], its wait for the mapping lock
/// counted by the remap meter.
fn remapping_in(ranges: &[(usize, usize)]) -> TimedRemapping {
    if !remap_meter::on() {
        return TimedRemapping(Some(victory_gpu::title_memory_remapping_in(ranges)));
    }
    let t = std::time::Instant::now();
    let r = victory_gpu::title_memory_remapping_in(ranges);
    let ns = t.elapsed().as_nanos() as u64;
    remap_meter::lock(ns);
    remap_meter::part(remap_meter::P_MAP_LOCK, ns);
    TimedRemapping(Some(r))
}

/// (V, remap meter) The change of mapping held; letting it go (our notes of
/// the change: guards, views, records) is timed into the call's parts.
struct TimedRemapping(Option<victory_gpu::Remapping>);

impl Drop for TimedRemapping {
    fn drop(&mut self) {
        if !remap_meter::on() {
            return;
        }
        let t = std::time::Instant::now();
        drop(self.0.take());
        remap_meter::part(remap_meter::P_NOTES, t.elapsed().as_nanos() as u64);
    }
}

/// Runs `f`, a system call, and counts it while the streaming log is on.
fn counted<T>(call: Call, f: impl FnOnce() -> T) -> T {
    if remap_meter::on() {
        let started = std::time::Instant::now();
        let done = f();
        let ns = started.elapsed().as_nanos() as u64;
        let kind = match call { Call::Unmap => 0, Call::Split => 1, Call::Map => 2, Call::Commit => 3 };
        remap_meter::sys(kind, ns);
        remap_meter::part(kind, ns);
        if super::dstorage::stream_log::on() {
            MAPPING[call as usize].fetch_add(1, Ordering::Relaxed);
            MAPPING[call as usize + 1].fetch_add(ns, Ordering::Relaxed);
        }
        return done;
    }
    if !super::dstorage::stream_log::on() {
        return f();
    }
    let started = std::time::Instant::now();
    let done = f();
    MAPPING[call as usize].fetch_add(1, Ordering::Relaxed);
    MAPPING[call as usize + 1].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    done
}

/// Counts a mapping call when dropped.
struct Costed {
    started: std::time::Instant,
    /// `None`: a copy of mappings.
    pages: Option<usize>,
    waited: u64,
    /// (C3, remap meter) Pages, also of a copy of mappings.
    meter_pages: usize,
    /// (V, remap meter) The call's own parts, if it is the outermost one.
    parts: Option<remap_meter::Parts>,
    /// (V, `VICTORY_MAP_QUIET`) A mapping call in progress, for our own
    /// protection changes on other threads.
    _in_map: Option<map_quiet::InMap>,
}

impl Costed {
    fn begin(pages: Option<usize>, what: &'static str, address: usize, len: usize) -> Option<Costed> {
        (super::dstorage::stream_log::on() || remap_meter::on() || map_quiet::on()).then(|| Costed {
            started: std::time::Instant::now(),
            pages,
            waited: 0,
            meter_pages: pages.unwrap_or(0),
            parts: remap_meter::on().then(|| remap_meter::Parts::begin(what, address, len)).flatten(),
            _in_map: map_quiet::on().then(map_quiet::InMap::begin),
        })
    }
}

impl Drop for Costed {
    fn drop(&mut self) {
        let ns = self.started.elapsed().as_nanos() as u64;
        if let Some(parts) = self.parts.take() {
            parts.end(ns, self.pages.unwrap_or(self.meter_pages));
        }
        if remap_meter::on() {
            remap_meter::call(self.pages.unwrap_or(self.meter_pages), ns, self.waited);
            if !super::dstorage::stream_log::on() {
                return;
            }
        }
        let Some(pages) = self.pages else {
            MAPPING[5].fetch_add(1, Ordering::Relaxed);
            MAPPING[6].fetch_add(ns, Ordering::Relaxed);
            return;
        };
        for (i, n) in [1, pages as u64, ns].into_iter().enumerate() {
            MAPPING[i].fetch_add(n, Ordering::Relaxed);
        }
        MAPPING[3].fetch_max(ns, Ordering::Relaxed);
        MAPPING[4].fetch_add(self.waited, Ordering::Relaxed);
    }
}

/// Whether a mapping change asks the host only for what it needs: no split
/// of a placeholder that is already the view's own, no commit of section
/// pages that are committed (`VICTORY_MAP_LEAN=0`: both every time, as
/// before).
///
/// All of the title's streaming goes through one thread, which maps the
/// pages of a batch and then submits it. Each call waits for the process's
/// one address-space lock, behind the guards' and the driver's: 200 to 400
/// us a page in the world against 10 in a menu, 2 to 4 s of every 5 s of
/// that thread (runs stream-1 and -2; all six stacks taken of it in the
/// opening were in these calls), so that models came as fast as their pages
/// could be mapped. A page took up to four calls (unmap, split, map,
/// commit), twice: the title maps a page into its pool and then has the GPU
/// map it into its tiled heap.
fn lean() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_MAP_LEAN").is_none_or(|v| v != "0"));
    *ON
}

/// The title's streaming thread runs below normal priority, as do sixteen
/// workers of ours when pipelines are made; on the console nothing else
/// wants its core. Called where that thread maps and submits: lifts a
/// thread that is below normal to above it (`VICTORY_STREAM_PRIORITY=0`:
/// left as the title set it).
pub(crate) fn lift_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_STREAM_PRIORITY").is_none_or(|v| v != "0"));
    if !*ON {
        return;
    }
    // SAFETY: plain calls on this thread.
    unsafe {
        let thread = GetCurrentThread();
        if matches!(GetThreadPriority(thread), -2..0) {
            SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
/// Renderer port 1 (`VICTORY_PORT_CONSTANTS=1`; the GPU side's half is
/// `victory_gpu`'s `port`): read here for xmem's part, [`arrange`].
fn port_on() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("VICTORY_PORT_CONSTANTS").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The title's reservations of 64 KiB pages for 32-bit graphics memory: base and bytes.
static LOW_RESERVATIONS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Pages given another place in their section so far by [`arrange`], and
/// the times it found a page that was not fresh.
static ARRANGED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Before `pages` are mapped at `base`: if `base` lies in a 32-bit graphics
/// reservation ([`LOW_RESERVATIONS`]) whose pages are all one section at least
/// as large, the page numbers are given the slices of that section in the
/// reservation's address order so the GPU side can open it as one heap.
fn arrange(base: usize, pages: &[usize]) {
    let Some((first, bytes)) = LOW_RESERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .find(|&(at, len)| at <= base && base + pages.len() * PAGE <= at + len)
    else {
        return;
    };
    let mut table = page_table_mut();
    let Some(section) = pages.first().and_then(|p| table.pages.get(p)).map(|s| s.section.clone()) else {
        return;
    };
    if section.pages * PAGE < bytes
        || !pages.iter().all(|p| table.pages.get(p).is_some_and(|s| Arc::ptr_eq(&s.section, &section)))
    {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 4 {
            tracing::warn!(
                at = format_args!("{base:#x}"),
                pages = pages.len(),
                section_pages = section.pages,
                reservation = bytes / PAGE,
                "renderer port 1: pages mapped into the 32-bit heap are not of one section as big as it: left where they are"
            );
        }
        return;
    }
    let fresh = |index: usize| section.committed[index / 64].load(Ordering::Acquire) >> (index % 64) & 1 == 0;
    let mut owner: HashMap<usize, usize> = table
        .pages
        .iter()
        .filter(|(_, s)| Arc::ptr_eq(&s.section, &section))
        .map(|(&number, s)| (s.index, number))
        .collect();
    let mut moved = 0u64;
    for (i, number) in pages.iter().enumerate() {
        let want = (base - first) / PAGE + i;
        let Some(have) = table.pages.get(number).map(|s| s.index) else {
            continue;
        };
        if have == want {
            continue;
        }
        if !fresh(have) || !fresh(want) {
            ARRANGED[1].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if let Some(other) = owner.get(&want).copied()
            && let Some(slice) = table.pages.get_mut(&other)
        {
            slice.index = have;
            owner.insert(have, other);
        }
        if let Some(slice) = table.pages.get_mut(number) {
            slice.index = want;
        }
        owner.insert(want, *number);
        moved += 1;
    }
    if moved > 0 {
        let all = ARRANGED[0].fetch_add(moved, Ordering::Relaxed) + moved;
        tracing::info!(
            at = format_args!("{base:#x}"),
            pages = pages.len(),
            moved,
            all,
            not_fresh = ARRANGED[1].load(Ordering::Relaxed),
            "renderer port 1: the title's 32-bit graphics pages given their section's slices in address order"
        );
    }
}

pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let _timed = soft::Timed::title();
    // Read before any lock is taken: a fault in title memory may wait for
    // the mapping call that holds them.
    // SAFETY: the title passes `n` page numbers, or null.
    let pages = (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) }.to_vec());
    if port_on()
        && let Some(pages) = &pages
    {
        arrange(base, pages);
    }
    // (F10, `VICTORY_MAP_DEFER`) Pending unmappings of these pages first, in
    // call order (none on the thread applying them).
    DEFER.before(base, n.saturating_mul(PAGE));
    let mut costed = Costed::begin(Some(n), "XMemMapPhysicalPages", base, n.saturating_mul(PAGE));
    let _called = victory_gpu::timeline::span("the title's XMemMapPhysicalPages");
    let _busy = Busy::begin();
    let mut physical = physical();
    if let Some(costed) = &mut costed {
        costed.waited = costed.started.elapsed().as_nanos() as u64;
    }
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    // (Neptune, `VICTORY_REMAP_SAME`) The pages are there already, each where
    // it is asked for, or nothing is there and nothing is asked for: no view
    // is taken away or made, and the notes of a remapping are not made, as
    // nothing they describe changed. The GPU side is told the bytes may have
    // changed (`kernel_will_write`), as a remapping told it (`=2`: told
    // nothing; `=0`: mapped again, as before).
    let mode = remap_same();
    if mode != 0 {
        let same = match &pages {
            Some(asked) => shows(&physical, base, asked),
            None => shows_nothing(&physical, base, n * PAGE),
        };
        same_count(n, pages.is_some(), same);
        if same {
            drop(physical);
            if mode == 1 && pages.is_some() {
                victory_gpu::kernel_will_write(base, n * PAGE);
            }
            return address;
        }
    }
    let _changing = remapping_in(&[affected(&physical, base, n * PAGE)]);
    map_runs::touch(base, n * PAGE);
    unmap(&mut physical, base, n * PAGE);
    let Some(pages) = pages else {
        return address;
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        pages.iter().map(|page| table.pages.get(page).cloned()).collect()
    };
    let mut i = 0;
    while i < n {
        let Some(first) = slices[i].clone() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + slices[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(slice, k)| {
                slice.as_ref().is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    let _timed = soft::Timed::title();
    lift_thread();
    if super::mapdefer::on() {
        // (F10, `VICTORY_MAP_DEFER`) Unmappings deferred, maps as below.
        return scatter_deferred(addresses, n, pages).into();
    }
    scatter_now(addresses, n, pages)
}

/// (F10, `VICTORY_MAP_DEFER`) The deferred unmappings of both views.
pub(crate) static DEFER: LazyLock<super::mapdefer::Deferrer> =
    LazyLock::new(|| super::mapdefer::Deferrer::new(apply_deferred, super::mapdefer::LIMIT_PAGES));

/// (F10) Applies a deferred unmapping as the call would have: the CPU view
/// page by page (`XMemMapPhysicalPages(at, 1, null)`), the GPU view as
/// `map_alias(at, 0, len)` in pieces of at most 16 views cut at view starts
/// (no view is cut that the whole range would not cut; the views' lock is
/// let go between pieces, so the frame thread's maps are not held up long).
fn apply_deferred(op: &super::mapdefer::Op) {
    use super::mapdefer::Op;
    match *op {
        Op::Cpu { at, pages } => {
            for k in 0..pages {
                XMemMapPhysicalPages((at + k * PAGE) as *mut c_void, 1, std::ptr::null());
            }
        }
        Op::Gpu { at, len } => {
            let end = at.saturating_add(len);
            let starts: Vec<usize> =
                if len == 0 { Vec::new() } else { physical().views.range(at + 1..end).map(|(a, _)| *a).collect() };
            let mut from = at;
            for (i, b) in starts.into_iter().enumerate() {
                if (i + 1) % 16 == 0 {
                    map_alias(from as u64, 0, (b - from) as u64);
                    from = b;
                }
            }
            map_alias(from as u64, 0, (end - from) as u64);
        }
    }
}

/// (F10, `VICTORY_MAP_DEFER`) `XMemMapPhysicalPagesScatter` with its
/// unmappings deferred (entries that follow on page by page as one
/// operation, applied page by page) and its maps done now by the fox path
/// ([`scatter_now`], side by side as `VICTORY_MAP_THREADS` says), after the
/// pending unmappings of their pages. An unmapping is deferred only after
/// the maps before it in the call are done (one may be of its page).
fn scatter_deferred(addresses: *const *mut c_void, n: usize, pages: *const usize) -> bool {
    use super::mapdefer::Op;
    // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
    let entry = |i: usize| unsafe {
        let page = if pages.is_null() { 0 } else { *pages.add(i) };
        (*addresses.add(i), page)
    };
    let mut run: Option<(usize, usize)> = None;
    let mut maps: Vec<(*mut c_void, usize)> = Vec::new();
    let mut ok = true;
    let flush_maps = |maps: &mut Vec<(*mut c_void, usize)>| {
        if maps.is_empty() {
            return true;
        }
        for &(at, _) in maps.iter() {
            DEFER.before(at as usize, PAGE);
        }
        let (a, p): (Vec<*mut c_void>, Vec<usize>) = maps.drain(..).unzip();
        scatter_now(a.as_ptr(), a.len(), p.as_ptr()) != 0
    };
    for i in 0..n {
        let (address, page) = entry(i);
        let at = address as usize;
        if page == 0 {
            ok &= flush_maps(&mut maps);
            match &mut run {
                Some((first, k)) if first.checked_add(*k * PAGE) == Some(at) => *k += 1,
                _ => {
                    if let Some((at, pages)) = run.take() {
                        DEFER.defer(Op::Cpu { at, pages });
                    }
                    run = Some((at, 1));
                }
            }
        } else {
            if let Some((at, pages)) = run.take() {
                DEFER.defer(Op::Cpu { at, pages });
            }
            maps.push((address, page));
        }
    }
    ok &= flush_maps(&mut maps);
    if let Some((at, pages)) = run.take() {
        DEFER.defer(Op::Cpu { at, pages });
    }
    super::mapdefer::line_now();
    ok
}

/// The fox base's `XMemMapPhysicalPagesScatter` after its checks, as it was.
fn scatter_now(addresses: *const *mut c_void, n: usize, pages: *const usize) -> BOOL {
    if side_by_side() > 1 && n > 1 {
        // Read before any lock is taken, as in `XMemMapPhysicalPages`.
        let wanted: Vec<(usize, usize)> = (0..n)
            // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
            .map(|i| unsafe { (*addresses.add(i) as usize, if pages.is_null() { 0 } else { *pages.add(i) }) })
            .collect();
        if wanted.iter().all(|w| w.0 % PAGE == 0) {
            return scatter(&wanted).into();
        }
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let _timed = soft::Timed::copies();
    map_alias_in(to, from, len, true)
}

/// [`map_alias`]; `alone`: not a range of [`map_aliases_now`], which counts
/// its runs itself (`VICTORY_MAP_RUNS`).
fn map_alias_in(to: u64, from: u64, len: u64, alone: bool) -> bool {
    if from != 0 {
        victory_gpu::loadmeter::gpu_view(to as usize, len as usize);
        super::pagekinds::gpu_view(to as usize, len as usize);
    }
    // (F10, `VICTORY_MAP_DEFER`) An unmapping (always a success here)
    // deferred; a map applies the pending unmappings of both its ranges first.
    if from == 0 && super::mapdefer::on() {
        DEFER.defer(super::mapdefer::Op::Gpu { at: to as usize, len: len as usize });
        return true;
    }
    DEFER.before(to as usize, len as usize);
    if from != 0 {
        DEFER.before(from as usize, len as usize);
    }
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut _costed = Costed::begin(None, "page mapping copy (map_alias)", to, len);
    if let Some(c) = &mut _costed {
        c.meter_pages = len.div_ceil(PAGE);
    }
    let _busy = Busy::begin();
    let mut physical = physical();
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = remapping_in(&[affected(&physical, to, len)]);
    if alone {
        map_runs::touch(to, len);
    }
    unmap(&mut physical, to, len);
    if from != 0 && map_runs::apply() && soft::mode() != soft::COPIES {
        // (V, `VICTORY_MAP_RUNS=1`) Pages consecutive in the section as one
        // view each.
        let mut k = 0;
        while k < len {
            let Some(slice) = physical.slice_at(from + k) else {
                return false;
            };
            let mut n = PAGE;
            while k + n < len
                && physical.slice_at(from + k + n).is_some_and(|s| Arc::ptr_eq(&s.section, &slice.section) && s.index == slice.index + n / PAGE)
            {
                n += PAGE;
            }
            if !map_runs::map_run(&mut physical, to + k, n, slice) {
                // As one view it did not go: page by page, as before.
                for j in (0..n).step_by(PAGE) {
                    let Some(slice) = physical.slice_at(from + k + j) else {
                        return false;
                    };
                    if !map(&mut physical, to + k + j, PAGE, slice) {
                        return false;
                    }
                }
            }
            k += n;
        }
        map_runs::verify(to, from, len);
        return true;
    }
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical.slice_at(at) else {
            return false;
        };
        // (V2, `VICTORY_SOFT_VIEWS=copies`) Kept in the books alone.
        if soft::keep(&mut physical, to + k, &slice) {
            continue;
        }
        let would = (soft::mode() == soft::VERIFY).then(|| slice.clone());
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
        if let Some(slice) = would {
            soft::would(to + k, &slice);
        }
    }
    true
}

/// [`map_alias`] of many ranges, in order.
pub fn map_aliases(ranges: &[(u64, u64, u64)]) -> bool {
    let _timed = soft::Timed::copies();
    if victory_gpu::loadmeter::on() || super::pagekinds::on() {
        for &(to, from, len) in ranges {
            if from != 0 {
                victory_gpu::loadmeter::gpu_view(to as usize, len as usize);
                super::pagekinds::gpu_view(to as usize, len as usize);
            }
        }
    }
    if super::mapdefer::on() {
        lift_thread();
        // (F10, `VICTORY_MAP_DEFER`) Unmappings deferred in order; the maps
        // between them go by the fox path after the pending unmappings of
        // their ranges (an unmapping after a map of the same page is
        // deferred only once the maps before it are done).
        let mut ok = true;
        let mut maps: Vec<(u64, u64, u64)> = Vec::new();
        let flush = |maps: &mut Vec<(u64, u64, u64)>| {
            if maps.is_empty() {
                return true;
            }
            for &(to, from, len) in maps.iter() {
                DEFER.before(to as usize, len as usize);
                DEFER.before(from as usize, len as usize);
            }
            let done = map_aliases_now(maps);
            maps.clear();
            done
        };
        for &(to, from, len) in ranges {
            if from == 0 {
                ok &= flush(&mut maps);
                DEFER.defer(super::mapdefer::Op::Gpu { at: to as usize, len: len as usize });
            } else {
                maps.push((to, from, len));
            }
        }
        ok &= flush(&mut maps);
        super::mapdefer::line_now();
        return ok;
    }
    map_aliases_now(ranges)
}

/// The fox base's [`map_aliases`], as it was.
fn map_aliases_now(ranges: &[(u64, u64, u64)]) -> bool {
    let _costed = Costed::begin(
        None,
        "page mapping copies (map_aliases)",
        ranges.first().map_or(0, |r| r.0 as usize),
        ranges.iter().map(|r| r.2 as usize).sum(),
    );
    lift_thread();
    // (V2, `VICTORY_SOFT_VIEWS=copies`) One page after the other: no system
    // call is left to share out.
    if side_by_side() <= 1 || map_runs::apply() || soft::mode() == soft::COPIES {
        if !map_runs::on() {
            return ranges.iter().all(|&(to, from, len)| map_alias(to, from, len));
        }
        // (V, `VICTORY_MAP_RUNS=verify`) One page after the other
        // (`VICTORY_MAP_THREADS=1`, the standing set's path; run-232 counted
        // nothing for want of this): what each page is to show, read before.
        let shown: Vec<(usize, Option<(usize, usize)>)> = {
            let physical = physical();
            ranges
                .iter()
                .flat_map(|&(to, from, len)| {
                    (0..len as usize).step_by(PAGE).map(move |k| (to as usize + k, (from != 0).then_some(from as usize + k)))
                })
                .map(|(to, from)| {
                    let what = from
                        .and_then(|from| physical.slice_at(from))
                        .map(|s| (Arc::as_ptr(&s.section) as usize, s.index));
                    (to, what)
                })
                .collect()
        };
        // (`VICTORY_MAP_RUNS=1`) Ranges that follow each other here and in
        // their source as one (the title's batches are one page a range).
        let joined: Vec<(u64, u64, u64)> = if map_runs::apply() {
            let mut out: Vec<(u64, u64, u64)> = Vec::new();
            for &(to, from, len) in ranges {
                match out.last_mut() {
                    Some(last) if from != 0 && last.1 != 0 && last.0 + last.2 == to && last.1 + last.2 == from => last.2 += len,
                    _ => out.push((to, from, len)),
                }
            }
            out
        } else {
            ranges.to_vec()
        };
        let ok = joined.iter().all(|&(to, from, len)| map_alias_in(to, from, len, false));
        map_runs::call(&shown);
        return ok;
    }
    // By the page: the address, and the one whose page it is to show.
    let pages: Vec<(usize, Option<usize>)> = ranges
        .iter()
        .flat_map(|&(to, from, len)| {
            (0..len as usize).step_by(PAGE).map(move |k| (to as usize + k, (from != 0).then_some(from as usize + k)))
        })
        .collect();
    if pages.iter().any(|p| p.0 % PAGE != 0 || p.1.is_some_and(|from| from % PAGE != 0)) {
        return ranges.iter().all(|&(to, from, len)| map_alias(to, from, len));
    }
    let mut ok = true;
    let mut at = 0;
    // (V, `VICTORY_MAP_RUNS=verify`) What each page is to show: section, page.
    let mut shown: Vec<(usize, Option<(usize, usize)>)> = Vec::new();
    let runs_on = map_runs::on();
    while at < pages.len() {
        let _busy = Busy::begin();
        let mut physical = physical();
        let mut ops: Vec<Op> = Vec::new();
        let mut touched = std::collections::HashSet::new();
        for &(to, from) in &pages[at..] {
            // A page the group changes already, or one it would be mapped
            // from: after it.
            if ops.len() == GROUP || touched.contains(&to) || from.is_some_and(|from| touched.contains(&from)) {
                break;
            }
            let slice = from.map(|from| physical.slice_at(from));
            ok &= !matches!(slice, Some(None));
            touched.insert(to);
            if runs_on {
                shown.push((to, slice.as_ref().and_then(|s| s.as_ref()).map(|s| (Arc::as_ptr(&s.section) as usize, s.index))));
            }
            ops.push(Op::new(to, slice.flatten()));
            at += 1;
        }
        let _changing = remapping_in(&affected_ops(&physical, &ops));
        ok &= change(&mut physical, ops);
    }
    if runs_on {
        map_runs::call(&shown);
    }
    ok
}

/// (V; `VICTORY_MAP_QUIET=1`, an experiment, default off; counted with
/// `VICTORY_REMAP_METER=1` too) Our own protection changes and write-watch
/// resets against the title's mapping calls. A mapping call waits in the
/// kernel for the process's address-space lock, which every protection change
/// takes as well (run-230: map/unmap 100-130 us each in the copies against
/// 2-12 us quiet; our guards make ~100,000 protection changes a second). The
/// entries of our own import table for `VirtualProtect`, `GetWriteWatch` and
/// `ResetWriteWatch` point here: each call is counted inside or outside a
/// mapping call of the title's in progress, and with the switch one of ours
/// on another thread waits (at most [`GATE_MS`]) until no mapping call is in
/// progress. The mapping thread's own calls never wait.
mod map_quiet {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
    use std::sync::{Condvar, Mutex};

    /// The longest one of ours waits at the gate.
    const GATE_MS: u64 = 20;

    pub(super) fn gate() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            let on = std::env::var("VICTORY_MAP_QUIET").is_ok_and(|v| v.trim() == "1");
            if on {
                tracing::info!(gate_ms = GATE_MS, "our protection changes wait while a mapping call of the title's is in progress (VICTORY_MAP_QUIET=1)");
            }
            on
        })
    }

    pub(super) fn on() -> bool {
        gate() || super::remap_meter::on()
    }

    /// Mapping calls in progress.
    static IN_MAP: AtomicUsize = AtomicUsize::new(0);
    static QUIET: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

    thread_local! {
        /// This thread is in a mapping call (its own protection changes pass).
        static MAPPING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// A mapping call of the title's in progress, while it lives.
    pub(super) struct InMap;

    impl InMap {
        pub(super) fn begin() -> InMap {
            arm();
            MAPPING.with(|m| m.set(m.get() + 1));
            IN_MAP.fetch_add(1, Relaxed);
            InMap
        }
    }

    impl Drop for InMap {
        fn drop(&mut self) {
            MAPPING.with(|m| m.set(m.get().saturating_sub(1)));
            if IN_MAP.fetch_sub(1, Relaxed) == 1 {
                let _held = QUIET.0.lock().unwrap_or_else(|e| e.into_inner());
                QUIET.1.notify_all();
            }
        }
    }

    /// Kinds of calls of ours: protection changes, write-watch resets,
    /// write-watch reads, VirtualQuery, ReadProcessMemory.
    const KINDS: usize = 5;
    const KIND_NAMES: [&str; KINDS] = ["protection changes", "write-watch resets", "write-watch reads", "VirtualQuery", "ReadProcessMemory"];
    /// Kinds of threads: the title's (unnamed), our submitting thread, the
    /// relay, DirectStorage workers, mapping helpers, our other threads.
    const THREADS: usize = 6;
    const THREAD_NAMES: [&str; THREADS] = ["title", "submit", "relay", "dstorage", "mapping", "other ours"];

    thread_local! {
        static THREAD_KIND: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }

    /// The thread's kind (`None` while its thread-locals are going: a call
    /// then goes uncounted).
    fn thread_kind() -> Option<usize> {
        THREAD_KIND.try_with(|k| {
            if k.get() == usize::MAX {
                let name = thread_name();
                k.set(if name.is_empty() || name == "main" {
                    0
                } else if name.starts_with("victory-submit") {
                    1
                } else if name.starts_with("victory-relay") || name.starts_with("land") {
                    2
                } else if name.starts_with("dstorage") {
                    3
                } else if name.starts_with("mapping") {
                    4
                } else {
                    5
                });
            }
            k.get()
        })
        .ok()
    }

    /// The thread's name as the system has it (Rust's names are set there).
    fn thread_name() -> String {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadDescription};
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: a plain query; the text it gives is freed here.
        unsafe {
            if GetThreadDescription(GetCurrentThread(), &mut text) < 0 || text.is_null() {
                return String::new();
            }
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text.cast());
            name
        }
    }

    /// Per kind and thread kind: calls and ns while a mapping call of the
    /// title's was in progress, the same outside, bytes, the longest ns.
    static STATS: [[[AtomicU64; 6]; THREADS]; KINDS] = [const { [const { [const { AtomicU64::new(0) }; 6] }; THREADS] }; KINDS];
    /// Per kind (the first two): waits at the gate, their ns, waits that
    /// gave up at [`GATE_MS`].
    static GATED: [[AtomicU64; 3]; 2] = [const { [const { AtomicU64::new(0) }; 3] }; 2];
    /// Calls of ours over a millisecond, logged so far.
    static SLOW_LOGGED: AtomicU64 = AtomicU64::new(0);

    /// Our calls in flight on threads not in a mapping call: how many, since
    /// when the count last changed, the ns with one or more in flight since
    /// the start, the start of each by thread id; the most in flight and the
    /// longest finished since the last mapping call began.
    struct Flights {
        active: usize,
        last: Option<std::time::Instant>,
        busy_ns: u64,
        starts: std::collections::BTreeMap<u32, std::time::Instant>,
    }

    static FLIGHTS: Mutex<Flights> = Mutex::new(Flights { active: 0, last: None, busy_ns: 0, starts: std::collections::BTreeMap::new() });
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    static LONGEST: AtomicU64 = AtomicU64::new(0);

    fn tid() -> u32 {
        // SAFETY: a plain query.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
    }

    fn busy_now(f: &Flights, now: std::time::Instant) -> u64 {
        f.busy_ns + if f.active > 0 { f.last.map_or(0, |l| (now - l).as_nanos() as u64) } else { 0 }
    }

    /// At a mapping call's start: the overlap clock now; the peak and the
    /// longest start over.
    pub(super) fn overlap_begin() -> u64 {
        let f = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
        PEAK.store(f.active, Relaxed);
        LONGEST.store(0, Relaxed);
        busy_now(&f, std::time::Instant::now())
    }

    /// At its end: ns with one of ours in flight elsewhere since `begun`,
    /// the most in flight at once, the longest of them (ns).
    pub(super) fn overlap_end(begun: u64) -> (u64, usize, u64) {
        let now = std::time::Instant::now();
        let f = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
        let open = f.starts.values().map(|s| (now - *s).as_nanos() as u64).max().unwrap_or(0);
        (busy_now(&f, now).saturating_sub(begun), PEAK.load(Relaxed), LONGEST.load(Relaxed).max(open))
    }

    /// Counts (and, kinds 0-1 with the gate, holds) one call of ours of kind
    /// `k` over `bytes`.
    fn around<T>(k: usize, bytes: usize, call: impl FnOnce() -> T) -> T {
        let (Ok(mapping), Some(th)) = (MAPPING.try_with(std::cell::Cell::get), thread_kind()) else {
            // A thread whose thread-locals are going: as it is.
            return call();
        };
        let mine = mapping > 0;
        if k < 2 && gate() && !mine && IN_MAP.load(Relaxed) > 0 {
            let g = &GATED[k];
            let t = std::time::Instant::now();
            let mut held = QUIET.0.lock().unwrap_or_else(|e| e.into_inner());
            let mut gave_up = false;
            while IN_MAP.load(Relaxed) > 0 {
                let left = std::time::Duration::from_millis(GATE_MS).saturating_sub(t.elapsed());
                if left.is_zero() {
                    gave_up = true;
                    break;
                }
                held = QUIET.1.wait_timeout(held, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            drop(held);
            g[0].fetch_add(1, Relaxed);
            g[1].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
            if gave_up {
                g[2].fetch_add(1, Relaxed);
            }
        }
        let during = IN_MAP.load(Relaxed) > 0 && !mine;
        let t = std::time::Instant::now();
        if !mine {
            let mut f = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
            f.busy_ns = busy_now(&f, t);
            f.active += 1;
            f.last = Some(t);
            f.starts.insert(tid(), t);
            PEAK.fetch_max(f.active, Relaxed);
        }
        let done = call();
        let end = std::time::Instant::now();
        let ns = (end - t).as_nanos() as u64;
        if !mine {
            let mut f = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
            f.busy_ns = busy_now(&f, end);
            f.active = f.active.saturating_sub(1);
            f.last = Some(end);
            f.starts.remove(&tid());
            LONGEST.fetch_max(ns, Relaxed);
        }
        let st = &STATS[k][th];
        let i = if during { 0 } else { 2 };
        st[i].fetch_add(1, Relaxed);
        st[i + 1].fetch_add(ns, Relaxed);
        st[4].fetch_add(bytes as u64, Relaxed);
        st[5].fetch_max(ns, Relaxed);
        if ns > 1_000_000 {
            let n = SLOW_LOGGED.fetch_add(1, Relaxed);
            if n < 300 || n.is_multiple_of(100) {
                tracing::info!(
                    call = KIND_NAMES[k],
                    thread = THREAD_NAMES[th],
                    ms = ns as f64 / 1e6,
                    bytes,
                    title_mapping_in_progress = IN_MAP.load(Relaxed) > 0,
                    n = n + 1,
                    "remap meter: a call of ours on the address space over 1 ms"
                );
            }
        }
        done
    }

    type Protect = unsafe extern "system" fn(*const c_void, usize, u32, *mut u32) -> i32;
    type GetWatch = unsafe extern "system" fn(u32, *const c_void, usize, *mut *mut c_void, *mut usize, *mut u32) -> u32;
    type ResetWatch = unsafe extern "system" fn(*const c_void, usize) -> u32;
    type Query = unsafe extern "system" fn(*const c_void, *mut c_void, usize) -> usize;
    type ReadMemory = unsafe extern "system" fn(*mut c_void, *const c_void, *mut c_void, usize, *mut usize) -> i32;
    /// The functions our import table named before (the system's, or a
    /// wrapper of the timeline's).
    static REAL: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];

    unsafe extern "system" fn protect(at: *const c_void, size: usize, new: u32, old: *mut u32) -> i32 {
        // SAFETY: the function our import table named.
        let real: Protect = unsafe { std::mem::transmute(REAL[0].load(Relaxed)) };
        // SAFETY: the caller's own call.
        around(0, size, || unsafe { real(at, size, new, old) })
    }

    unsafe extern "system" fn query(at: *const c_void, info: *mut c_void, len: usize) -> usize {
        // SAFETY: as above.
        let real: Query = unsafe { std::mem::transmute(REAL[3].load(Relaxed)) };
        // SAFETY: the caller's own call.
        around(3, 0, || unsafe { real(at, info, len) })
    }

    unsafe extern "system" fn read_memory(process: *mut c_void, at: *const c_void, to: *mut c_void, size: usize, done: *mut usize) -> i32 {
        // SAFETY: as above.
        let real: ReadMemory = unsafe { std::mem::transmute(REAL[4].load(Relaxed)) };
        // SAFETY: the caller's own call.
        around(4, size, || unsafe { real(process, at, to, size, done) })
    }

    unsafe extern "system" fn get_watch(flags: u32, at: *const c_void, size: usize, found: *mut *mut c_void, count: *mut usize, g: *mut u32) -> u32 {
        // SAFETY: as above.
        let real: GetWatch = unsafe { std::mem::transmute(REAL[1].load(Relaxed)) };
        // A reset (WRITE_WATCH_FLAG_RESET) and a read apart.
        // SAFETY: the caller's own call.
        around(if flags & 1 != 0 { 1 } else { 2 }, size, || unsafe { real(flags, at, size, found, count, g) })
    }

    unsafe extern "system" fn reset_watch(at: *const c_void, size: usize) -> u32 {
        // SAFETY: as above.
        let real: ResetWatch = unsafe { std::mem::transmute(REAL[2].load(Relaxed)) };
        // SAFETY: the caller's own call.
        around(1, size, || unsafe { real(at, size) })
    }

    /// Points our own import table's entries at the functions above, once.
    fn arm() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: our own module's mapped image, as the loader left it.
            let changed = unsafe { wrap() };
            tracing::info!(changed, "our VirtualProtect / GetWriteWatch / ResetWriteWatch / VirtualQuery / ReadProcessMemory imports counted (VICTORY_MAP_QUIET / VICTORY_REMAP_METER)");
        });
    }

    /// # Safety
    /// The process's module is mapped as the loader left it.
    unsafe fn wrap() -> usize {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
        let names: [(&[u8], usize); 5] = [
            (b"VirtualProtect", protect as *const () as usize),
            (b"GetWriteWatch", get_watch as *const () as usize),
            (b"ResetWriteWatch", reset_watch as *const () as usize),
            (b"VirtualQuery", query as *const () as usize),
            (b"ReadProcessMemory", read_memory as *const () as usize),
        ];
        // SAFETY: a plain query.
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        let mut changed = 0;
        // SAFETY: the headers, the import directory and the tables it names
        // lie in the mapped image; an entry is eight bytes, aligned, made
        // writable for the store.
        unsafe {
            let at = |rva: u32| base + rva as usize;
            let read = |at: usize| (at as *const u32).read_unaligned();
            let headers = at(read(base + 0x3c));
            let mut descriptor = at(read(headers + 0x18 + 0x70 + 8));
            loop {
                let (names_rva, first) = (read(descriptor), read(descriptor + 16));
                if first == 0 {
                    break;
                }
                let names_rva = if names_rva != 0 { names_rva } else { first };
                for i in 0.. {
                    let entry = (at(names_rva) as *const u64).add(i).read_unaligned();
                    if entry == 0 {
                        break;
                    }
                    if entry >> 63 != 0 {
                        continue;
                    }
                    let name = std::ffi::CStr::from_ptr((at(entry as u32) + 2) as *const _);
                    let Some(k) = names.iter().position(|n| n.0 == name.to_bytes()) else { continue };
                    let slot = (at(first) as *mut usize).add(i);
                    let now = slot.cast::<AtomicUsize>().as_ref().map_or(0, |s| s.load(Relaxed));
                    if now == 0 || now == names[k].1 {
                        continue;
                    }
                    if REAL[k].load(Relaxed) == 0 {
                        REAL[k].store(now, Relaxed);
                    }
                    let (mut old, mut ignored) = (0, 0);
                    if VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) != 0 {
                        (*slot.cast::<AtomicUsize>()).store(names[k].1, Relaxed);
                        VirtualProtect(slot.cast(), 8, old, &mut ignored);
                        changed += 1;
                    }
                }
                descriptor += 20;
            }
        }
        changed
    }

    /// For the remap meter's line.
    pub(super) fn line() -> String {
        if !on() {
            return String::new();
        }
        let ms = |v: u64| v as f64 / 1e6;
        let mut out = String::from("; ours on the address space (during the title's mapping calls / outside)");
        for (k, name) in KIND_NAMES.iter().enumerate() {
            let per: Vec<[u64; 6]> = (0..THREADS).map(|t| std::array::from_fn(|i| STATS[k][t][i].swap(0, Relaxed))).collect();
            let sum = |i: usize| per.iter().map(|p| p[i]).sum::<u64>();
            let longest = per.iter().map(|p| p[5]).max().unwrap_or(0);
            out.push_str(&format!(
                "; {name} {} / {} calls, {:.1} / {:.1} ms, {:.0} MB, longest {:.2} ms",
                sum(0),
                sum(2),
                ms(sum(1)),
                ms(sum(3)),
                sum(4) as f64 / 1048576.0,
                ms(longest)
            ));
            let by: Vec<String> = per
                .iter()
                .enumerate()
                .filter(|(_, p)| p[0] + p[2] > 0)
                .map(|(t, p)| format!("{} {} {:.1} ms", THREAD_NAMES[t], p[0] + p[2], ms(p[1] + p[3])))
                .collect();
            if !by.is_empty() {
                out.push_str(&format!(" ({})", by.join(", ")));
            }
        }
        if gate() {
            for (k, name) in ["protection changes", "write-watch resets"].iter().enumerate() {
                let g: Vec<u64> = GATED[k].iter().map(|a| a.swap(0, Relaxed)).collect();
                out.push_str(&format!("; {name} held at the gate {} ({:.1} ms, {} gave up at {GATE_MS} ms)", g[0], ms(g[1]), g[2]));
            }
        }
        out.push_str(&format!(" (VICTORY_MAP_QUIET={})", u8::from(gate())));
        out
    }
}

/// (V; `VICTORY_MAP_RUNS=verify`, default off; with `VICTORY_REMAP_METER=1`)
/// What the GPU page-mapping copies (`CopyPageMappingsBatchX`) would cost if
/// the pages of one call that are consecutive both in the address range and
/// in one section were mapped as one view each, and unmapped as one
/// (run-230: 114 of 122 mapping calls over 50 ms were these copies, a system
/// call per 64 KiB page: 59,152 unmaps at 131 us and 49,244 maps at 100 us).
/// Nothing is done differently: the per-page path runs as before, and a
/// record of the views runs would have made is kept beside it, so that every
/// later change of a page inside such a view is counted: as the whole view
/// (one unmap), or a part of it (an unmap of the view, a split, and the rest
/// mapped again). The figures on the remap meter's line: calls, pages, runs,
/// system calls of the per-page path and estimated with runs, changes that
/// would have cut a run.
mod map_runs {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    use super::PAGE;

    /// Tests turn the count on without the switches.
    #[cfg(test)]
    pub(super) static FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub(super) fn on() -> bool {
        #[cfg(test)]
        if FORCED.load(Relaxed) {
            return true;
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            let asked = std::env::var("VICTORY_MAP_RUNS").ok();
            let on = matches!(asked.as_deref().map(str::trim), Some("verify") | Some("1")) && super::remap_meter::on();
            if asked.is_some() && !on {
                tracing::warn!("VICTORY_MAP_RUNS needs VICTORY_REMAP_METER=1: off");
            }
            on
        })
    }

    /// (`VICTORY_MAP_RUNS=1`) The copies map runs as one view each (with
    /// `=verify` only counted). Off for the rest of the run after a check
    /// found a page that does not show what its source shows.
    pub(super) fn apply() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        #[cfg(test)]
        if APPLY.load(Relaxed) {
            return !BROKEN.load(Relaxed);
        }
        *ON.get_or_init(|| {
            let on = on() && std::env::var("VICTORY_MAP_RUNS").is_ok_and(|v| v.trim() == "1");
            if on {
                tracing::info!("the GPU's page-mapping copies map pages consecutive in the section as one view each (VICTORY_MAP_RUNS=1)");
            }
            on
        }) && !BROKEN.load(Relaxed)
    }

    #[cfg(test)]
    pub(super) static APPLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// (`=1`) Since the last line: runs mapped as one view, their pages;
    /// placeholders joined for them; checks made, pages checked, failed.
    static APPLIED: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

    /// Maps `len` bytes at `base` from `slice` on as one view: the
    /// placeholders the page-by-page unmapping left there are joined first
    /// (`MEM_COALESCE_PLACEHOLDERS`) when they lie exactly over the run.
    pub(super) fn map_run(physical: &mut super::Physical, base: usize, len: usize, slice: super::Slice) -> bool {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        if len > PAGE {
            // The holes over the run, in order, if they tile it exactly.
            let mut holes: Vec<(usize, usize)> = Vec::new();
            let mut at = base;
            while at < base + len
                && let Some(&l) = physical.holes.get(&at)
            {
                holes.push((at, l));
                at += l;
            }
            let tiles = holes.len() > 1 && at == base + len;
            if tiles {
                // SAFETY: placeholders of ours exactly over the range.
                let joined = super::counted(super::Call::Split, || unsafe {
                    VirtualFree(base as *mut std::ffi::c_void, len, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */)
                }) != 0;
                if joined {
                    for h in &holes {
                        physical.holes.remove(&h.0);
                    }
                    physical.holes.insert(base, len);
                    APPLIED[2].fetch_add(1, Relaxed);
                }
            }
        }
        let done = super::map(physical, base, len, slice);
        if done && len > PAGE {
            APPLIED[0].fetch_add(1, Relaxed);
            APPLIED[1].fetch_add((len / PAGE) as u64, Relaxed);
            RUNS.lock().unwrap_or_else(|e| e.into_inner()).insert(base, len);
        }
        done
    }

    /// (`=1`) One call in 16: every page of `[to, to + len)` shows what the
    /// same page of `[from, from + len)` shows (its first and last eight
    /// bytes), read-write, committed. A difference turns runs off for the
    /// rest of the run.
    pub(super) fn verify(to: usize, from: usize, len: usize) {
        static N: AtomicU64 = AtomicU64::new(0);
        if !N.fetch_add(1, Relaxed).is_multiple_of(16) {
            return;
        }
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery};
        APPLIED[3].fetch_add(1, Relaxed);
        for k in (0..len).step_by(PAGE) {
            // SAFETY: plain queries; a read only of committed read-write pages.
            let same = unsafe {
                let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                let asked = VirtualQuery((to + k) as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>());
                asked != 0
                    && info.State == MEM_COMMIT
                    && info.Protect == PAGE_READWRITE
                    && [0, PAGE - 8].iter().all(|&o| {
                        std::ptr::read_volatile((to + k + o) as *const u64) == std::ptr::read_volatile((from + k + o) as *const u64)
                    })
            };
            APPLIED[4].fetch_add(1, Relaxed);
            if !same {
                APPLIED[5].fetch_add(1, Relaxed);
                BROKEN.store(true, Relaxed);
                tracing::error!(
                    address = format_args!("{:#x}", to + k),
                    "a page mapped as part of a run does not show what its source shows: runs are off for the rest of the run (VICTORY_MAP_RUNS=1)"
                );
                return;
            }
        }
    }

    /// The views runs would have made and that still stand: first address, bytes.
    static RUNS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

    /// Since the last line: calls, pages, runs mapped, system calls of the
    /// per-page path, system calls estimated with runs; later changes that
    /// took a whole run, that cut one; pages in runs mapped.
    static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    const CALLS: usize = 0;
    const PAGES: usize = 1;
    const RUNS_MAPPED: usize = 2;
    const SYS_NOW: usize = 3;
    const SYS_RUNS: usize = 4;
    const WHOLE: usize = 5;
    const CUT: usize = 6;
    const IN_RUNS: usize = 7;

    /// The runs that `pages` (addresses) touch, taken out of the record: per
    /// run, its pages and how many of them the change touches. Pieces left
    /// of a cut run go back as runs (two pages or more).
    fn take(pages: &[usize]) -> Vec<(usize, usize, usize)> {
        let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let mut hit: BTreeMap<usize, (usize, Vec<usize>)> = BTreeMap::new();
        for &at in pages {
            if let Some((&base, &len)) = runs.range(..=at).next_back()
                && at < base + len
            {
                hit.entry(base).or_insert((len, Vec::new())).1.push(at);
            }
        }
        let mut out = Vec::new();
        for (base, (len, mut touched)) in hit {
            runs.remove(&base);
            touched.sort_unstable();
            touched.dedup();
            let n = len / PAGE;
            out.push((base, n, touched.len()));
            // What stays of it: the stretches between the pages touched.
            let mut from = base;
            for &t in touched.iter().chain(std::iter::once(&(base + len))) {
                if t >= from + 2 * PAGE {
                    runs.insert(from, t - from);
                }
                from = t + PAGE;
            }
        }
        out
    }

    /// A change of `pages` by a path that is not the copies' (counted as cuts
    /// or whole runs; nothing mapped as runs).
    pub(super) fn touch(base: usize, len: usize) {
        if !on() {
            return;
        }
        let pages: Vec<usize> = (base..base + len).step_by(PAGE).collect();
        for (_, n, touched) in take(&pages) {
            STATS[if touched == n { WHOLE } else { CUT }].fetch_add(1, Relaxed);
        }
    }

    /// One call of the copies: each page and what it is to show (section,
    /// page), in order; its system calls are the meter's of this call so far.
    pub(super) fn call(shown: &[(usize, Option<(usize, usize)>)]) {
        let [unmaps, splits, maps] = super::remap_meter::sys_now();
        let addresses: Vec<usize> = shown.iter().map(|s| s.0).collect();
        let hit = take(&addresses);
        // With runs: one unmap a run taken whole; a run cut: its unmap, a
        // split, and its pieces mapped again (two at most); pages in no run
        // as now (each its own view, one unmap each).
        let mut in_runs = 0u64;
        let mut sys_unmap = 0u64;
        for &(_, n, touched) in &hit {
            in_runs += touched as u64;
            sys_unmap += if touched == n { 1 } else { 3 + 1 };
            STATS[if touched == n { WHOLE } else { CUT }].fetch_add(1, Relaxed);
        }
        let unmaps_runs = unmaps.saturating_sub(in_runs) + sys_unmap;
        // The maps: one a run of pages consecutive here and in one section.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut last: Option<(usize, usize, usize)> = None;
        for &(at, what) in shown {
            match (what, last) {
                (Some((section, index)), Some((at0, s0, i0))) if at == at0 + PAGE && section == s0 && index == i0 + 1 => {
                    if let Some(r) = runs.last_mut() {
                        r.1 += PAGE;
                    }
                    last = Some((at, section, index));
                }
                (Some((section, index)), _) => {
                    runs.push((at, PAGE));
                    last = Some((at, section, index));
                }
                (None, _) => last = None,
            }
        }
        let mut record = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let mut paged = 0u64;
        for &(at, len) in &runs {
            if len >= 2 * PAGE {
                record.insert(at, len);
                paged += (len / PAGE) as u64;
            }
        }
        drop(record);
        let maps_runs = runs.len() as u64;
        // Splits: one a run at most.
        let splits_runs = splits.min(maps_runs);
        for (i, v) in [
            (CALLS, 1),
            (PAGES, shown.len() as u64),
            (RUNS_MAPPED, maps_runs),
            (SYS_NOW, unmaps + splits + maps),
            (SYS_RUNS, unmaps_runs + splits_runs + maps_runs),
            (IN_RUNS, paged),
        ] {
            STATS[i].fetch_add(v, Relaxed);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A run cut in its middle leaves its two ends as runs (two pages or
        /// more), and a run touched whole goes.
        /// The tests of the record of runs, one at a time.
        static SERIAL: Mutex<()> = Mutex::new(());

        /// The real path of the GPU's page-mapping copies, side by side and
        /// one page after the other (`VICTORY_MAP_THREADS=1`): both are
        /// counted, and a page of a run unmapped alone later cuts the run.
        #[test]
        fn the_copies_are_counted_on_both_paths() {
            use super::super::{XMemAllocatePhysicalPages, XMemFreePhysicalPages, XMemMapPhysicalPages, XMemVirtualAlloc};
            let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            FORCED.store(true, Relaxed);
            const N: usize = 16;
            let (mut pages, mut given) = (vec![0usize; N], N);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()), 1);
            let reserve = |n: usize| {
                XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, super::super::MEM_RESERVE | super::super::PAGES_64K, 0, super::super::PAGE_READWRITE) as usize
            };
            let (a, b) = (reserve(N), reserve(2 * N));
            assert!(a != 0 && b != 0);
            assert_eq!(XMemMapPhysicalPages(a as *mut _, N, pages.as_ptr()) as usize, a);
            let [calls0, pages0] = [CALLS, PAGES].map(|i| STATS[i].load(Relaxed));
            // Side by side, then one page after the other.
            assert!(super::super::map_aliases(&[(b as u64, a as u64, (N * PAGE) as u64)]));
            super::super::ONE_AT_A_TIME.with(|o| o.set(true));
            assert!(super::super::map_aliases(&[((b + N * PAGE) as u64, a as u64, (N * PAGE) as u64)]));
            super::super::ONE_AT_A_TIME.with(|o| o.set(false));
            assert!(STATS[CALLS].load(Relaxed) >= calls0 + 2, "both paths counted");
            assert!(STATS[PAGES].load(Relaxed) >= pages0 + 2 * N as u64);
            // The copies show what the pages hold.
            // SAFETY: mapped read-write.
            unsafe {
                (a as *mut u64).write(0x5a5a);
                assert_eq!(((b + N * PAGE) as *const u64).read(), 0x5a5a);
            }
            let standing = RUNS.lock().unwrap_or_else(|e| e.into_inner()).range(b..b + 2 * N * PAGE).count();
            let cut0 = STATS[CUT].load(Relaxed);
            assert!(super::super::map_alias((b + 3 * PAGE) as u64, 0, PAGE as u64));
            if standing > 0 {
                assert!(STATS[CUT].load(Relaxed) > cut0, "a page unmapped alone cuts its run");
            }
            assert!(super::super::map_aliases(&[(b as u64, 0, (2 * N * PAGE) as u64)]));
            assert_eq!(XMemMapPhysicalPages(a as *mut _, N, std::ptr::null()) as usize, a);
            assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
            FORCED.store(false, Relaxed);
        }

        /// (`=1`) The copies mapped as runs show, page by page, what their
        /// sources show; a page unmapped alone later cuts its run and the
        /// rest still shows its source; all of it unmapped leaves nothing.
        #[test]
        fn runs_mapped_as_one_view_show_their_pages() {
            use super::super::{XMemAllocatePhysicalPages, XMemFreePhysicalPages, XMemMapPhysicalPages, XMemVirtualAlloc};
            let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            FORCED.store(true, Relaxed);
            APPLY.store(true, Relaxed);
            const N: usize = 24;
            let (mut pages, mut given) = (vec![0usize; N], N);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()), 1);
            let reserve = |n: usize| {
                XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, super::super::MEM_RESERVE | super::super::PAGES_64K, 0, super::super::PAGE_READWRITE) as usize
            };
            let (a, b) = (reserve(N), reserve(N));
            assert_eq!(XMemMapPhysicalPages(a as *mut _, N, pages.as_ptr()) as usize, a);
            let page = |base: usize, k: usize| (base + k * PAGE) as *mut u64;
            // SAFETY: mapped read-write.
            unsafe {
                for k in 0..N {
                    page(a, k).write(1000 + k as u64);
                }
            }
            let runs0 = APPLIED[0].load(Relaxed);
            // Twice: over nothing, then over itself (views there already).
            for _ in 0..2 {
                assert!(super::super::map_aliases(&[(b as u64, a as u64, (N * PAGE) as u64)]));
                // SAFETY: mapped read-write.
                unsafe {
                    assert!((0..N).all(|k| page(b, k).read() == 1000 + k as u64), "every page shows its source");
                }
            }
            assert!(APPLIED[0].load(Relaxed) > runs0 || RUNS.lock().unwrap().range(b..b + N * PAGE).next().is_none());
            let largest = super::super::physical().views.range(b..b + N * PAGE).map(|(_, v)| v.0).max().unwrap_or(0);
            eprintln!("largest view over the copy: {} pages; runs mapped {}", largest / PAGE, APPLIED[0].load(Relaxed) - runs0);
            // The title's batches: a page a range. They are joined, mapped as
            // runs, and show their pages (run-242 made no run for want of it).
            let one_by_one: Vec<(u64, u64, u64)> =
                (0..N).map(|k| ((b + k * PAGE) as u64, (a + k * PAGE) as u64, PAGE as u64)).collect();
            assert!(super::super::map_aliases(&one_by_one));
            // SAFETY: mapped read-write.
            unsafe {
                assert!((0..N).all(|k| page(b, k).read() == 1000 + k as u64));
            }
            let largest = super::super::physical().views.range(b..b + N * PAGE).map(|(_, v)| v.0).max().unwrap_or(0);
            assert!(largest > PAGE, "one-page ranges joined into a run of {} pages", largest / PAGE);
            // One page of a run unmapped alone: the rest still shows its source.
            assert!(super::super::map_alias((b + 5 * PAGE) as u64, 0, PAGE as u64));
            // SAFETY: mapped read-write, the one page not.
            unsafe {
                assert!((0..N).filter(|&k| k != 5).all(|k| page(b, k).read() == 1000 + k as u64));
                page(b, 7).write(77);
                assert_eq!(page(a, 7).read(), 77, "one memory under both");
            }
            // SAFETY: a plain query.
            let state = unsafe {
                let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                windows_sys::Win32::System::Memory::VirtualQuery(page(b, 5).cast(), &mut info, size_of_val(&info));
                info.State
            };
            assert_eq!(state, super::super::MEM_RESERVE);
            assert!(super::super::map_aliases(&[(b as u64, 0, (N * PAGE) as u64)]));
            assert!(super::super::physical().views.range(b..b + N * PAGE).next().is_none());
            assert_eq!(XMemMapPhysicalPages(a as *mut _, N, std::ptr::null()) as usize, a);
            assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
            APPLY.store(false, Relaxed);
            FORCED.store(false, Relaxed);
        }

        #[test]
        fn cutting_a_run_leaves_its_ends() {
            let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            RUNS.lock().unwrap().clear();
            RUNS.lock().unwrap().insert(0x10_0000, 8 * PAGE);
            RUNS.lock().unwrap().insert(0x80_0000, 2 * PAGE);
            let hit = take(&[0x10_0000 + 3 * PAGE, 0x80_0000, 0x80_0000 + PAGE]);
            assert_eq!(hit, vec![(0x10_0000, 8, 1), (0x80_0000, 2, 2)]);
            let runs = RUNS.lock().unwrap().clone();
            assert_eq!(runs.into_iter().collect::<Vec<_>>(), vec![(0x10_0000, 3 * PAGE), (0x10_0000 + 4 * PAGE, 4 * PAGE)]);
        }
    }

    /// For the meter's line.
    pub(super) fn line() -> String {
        if !on() {
            return String::new();
        }
        let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Relaxed)).collect();
        let standing = RUNS.lock().unwrap_or_else(|e| e.into_inner()).len();
        let a: Vec<u64> = APPLIED.iter().map(|x| x.swap(0, Relaxed)).collect();
        let applied = if apply() || BROKEN.load(Relaxed) {
            format!(
                " | runs mapped as one view (VICTORY_MAP_RUNS=1{}): {} runs of {} pages, {} placeholders joined for them; checks {} of {} pages, {} failed",
                if BROKEN.load(Relaxed) { ", OFF after a failed check" } else { "" },
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                a[5]
            )
        } else {
            String::new()
        };
        format!(
            "; runs (VICTORY_MAP_RUNS): page mapping copies {} of {} pages would be {} runs ({} pages in runs of two or more); \
             system calls {} page by page, about {} with runs ({:.1} and {:.1} a call); later changes of a run's pages: {} whole, {} cutting it; \
             runs standing {}",
            s[CALLS],
            s[PAGES],
            s[RUNS_MAPPED],
            s[IN_RUNS],
            s[SYS_NOW],
            s[SYS_RUNS],
            s[SYS_NOW] as f64 / s[CALLS].max(1) as f64,
            s[SYS_RUNS] as f64 / s[CALLS].max(1) as f64,
            s[WHOLE],
            s[CUT],
            standing,
        ) + &applied
    }
}

/// Maps (page number not 0) or unmaps each address; false if a page is not
/// the title's or the host refuses one.
fn scatter(wanted: &[(usize, usize)]) -> bool {
    let mut ok = true;
    let mut at = 0;
    while at < wanted.len() {
        let _busy = Busy::begin();
        let mut costed = Costed::begin(None, "XMemMapPhysicalPagesScatter", wanted.get(at).map_or(0, |w| w.0), 0);
        let mut ops: Vec<Op> = Vec::new();
        let mut physical = physical();
        if let Some(costed) = &mut costed {
            costed.waited = costed.started.elapsed().as_nanos() as u64;
        }
        // (`VICTORY_REMAP_SAME`) Pages there already, told after the change.
        let mut same_at: Vec<usize> = Vec::new();
        let mode = remap_same();
        {
            let table = page_table();
            let mut touched = std::collections::HashSet::new();
            for &(to, page) in &wanted[at..] {
                if ops.len() == GROUP || !touched.insert(to) {
                    break;
                }
                let slice = table.pages.get(&page).cloned();
                if page != 0 && slice.is_none() {
                    tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
                    ok = false;
                }
                at += 1;
                if mode != 0 {
                    let there = physical.slice_at(to);
                    let same = match (&slice, &there) {
                        (Some(a), Some(b)) => Arc::ptr_eq(&a.section, &b.section) && a.index == b.index,
                        (None, None) => page == 0 && shows_nothing(&physical, to, PAGE),
                        _ => false,
                    };
                    same_count(1, page != 0, same);
                    if same {
                        if page != 0 {
                            same_at.push(to);
                        }
                        continue;
                    }
                }
                ops.push(Op::new(to, slice));
            }
        }
        if let Some(costed) = &mut costed {
            costed.pages = Some(ops.len());
        }
        if !ops.is_empty() {
            let _changing = remapping_in(&affected_ops(&physical, &ops));
            for op in &ops {
                map_runs::touch(op.at, PAGE);
            }
            ok &= change(&mut physical, ops);
        }
        drop(physical);
        if mode == 1 {
            for to in same_at {
                victory_gpu::kernel_will_write(to, PAGE);
            }
        }
    }
    ok
}

/// (Neptune) What a mapping of pages that are shown there already does
/// (`VICTORY_REMAP_SAME`; fox port: default 0 = mapped again as the fox base
/// does, until an A/B run; our tree's default was 1): 1: no view is taken away or made,
/// and the GPU side is told the bytes may have changed; 2: nothing; 0: the
/// pages are mapped again. The title's CPU view only (`XMemMapPhysicalPages`
/// and its scatter form); the GPU view's copies of mappings go as before.
fn remap_same() -> u8 {
    static MODE: LazyLock<u8> =
        LazyLock::new(|| std::env::var("VICTORY_REMAP_SAME").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0));
    *MODE
}

/// Whether `[base, base + pages)` shows `pages` already, page for page (none
/// unknown).
fn shows(physical: &Physical, base: usize, pages: &[usize]) -> bool {
    let table = page_table();
    pages.iter().enumerate().all(|(i, page)| {
        let (Some(want), Some(there)) = (table.pages.get(page), physical.slice_at(base + i * PAGE)) else {
            return false;
        };
        Arc::ptr_eq(&want.section, &there.section) && want.index == there.index
    })
}

/// Whether no view shows any of `[base, base + len)`.
fn shows_nothing(physical: &Physical, base: usize, len: usize) -> bool {
    let before = physical.views.range(..=base).next_back().is_some_and(|(at, (size, _))| at + size > base);
    !before && physical.views.range(base..base.saturating_add(len)).next().is_none()
}

/// (Neptune) Since the last line: pages asked to be mapped or unmapped, of
/// them there already, of nothing unmapped (nothing done).
static SAME: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Counts `n` pages asked (`mapped`: pages, not an unmapping; `same`: there
/// already), with a line every five seconds.
fn same_count(n: usize, mapped: bool, same: bool) {
    SAME[0].fetch_add(n as u64, Ordering::Relaxed);
    if same {
        SAME[if mapped { 1 } else { 2 }].fetch_add(n as u64, Ordering::Relaxed);
    }
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST.try_lock() else {
        return;
    };
    let now = std::time::Instant::now();
    let secs = now.duration_since(*last.get_or_insert(now)).as_secs_f64();
    if secs < 5.0 {
        return;
    }
    *last = Some(now);
    let w = [0, 1, 2].map(|i| SAME[i].swap(0, Ordering::Relaxed) as f64 / secs);
    tracing::info!(
        "title memory mappings asked for, a second over {secs:.0} s: {:.0} pages mapped or unmapped ({:.0} of them there already, {:.0} of nothing unmapped: nothing done) (VICTORY_REMAP_SAME={})",
        w[0],
        w[1],
        w[2],
        remap_same()
    );
}

/// Pages changed under the locks at once.
const GROUP: usize = 64;

/// How many threads map a group of pages side by side (`VICTORY_MAP_THREADS`;
/// 8 unless set; 1: the caller alone, a page after the other; `auto`: 1 while
/// mapping takes little of the time, [`WIDE`] while the title streams as fast
/// as its pages are mapped, [`Busy`]).
///
/// shortcut: `auto` is not the default. The intro's diner was there in time
/// with it (one run, textures-d3), but in the dorm, where each call takes 0.3
/// ms of something that only one at a time gets (until a step of the mission,
/// after which it takes 0.01), 16 threads map no more pages than one, and the
/// groups hold the mapping lock: 260 ms a frame against 125 (textures-d4, -d2).
///
/// Mapping and unmapping a view wait for a lock that every protection change
/// of a view takes too, and our guards make 100,000 of those a second, from
/// many threads, in the world: 0.2 to 4 ms a page. One thread stands in that
/// queue once at a time. In a bench (60,000 views, five threads changing
/// protections) one thread mapped 1,500 pages a second, eight 30,000.
fn side_by_side() -> usize {
    // (V) Tests of the one-page-after-the-other path.
    #[cfg(test)]
    if ONE_AT_A_TIME.with(std::cell::Cell::get) {
        return 1;
    }
    // Diagnostics: 1 and 8 by turns.
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(on) = victory_gpu::switched("VICTORY_MAP_THREADS") {
        return if on { 8 } else { 1 };
    }
    match asked_threads() {
        Some(n) => n,
        None if BUSY.2.load(Ordering::Relaxed) => WIDE,
        None => 1,
    }
}

#[cfg(test)]
thread_local! {
    /// (V) This test thread maps one page after the other ([`side_by_side`] 1).
    static ONE_AT_A_TIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Threads while the title streams fast: they stand in the host's queue side
/// by side, each for a millisecond or two a page in the world's first seconds.
const WIDE: usize = 16;

/// `VICTORY_MAP_THREADS` as a number; `None`: by how much there is to map.
fn asked_threads() -> Option<usize> {
    static ASKED: LazyLock<Option<usize>> = LazyLock::new(|| {
        match std::env::var("VICTORY_MAP_THREADS") {
            Ok(v) if v.trim() == "auto" => None,
            v => Some(v.ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8).clamp(1, 32)),
        }
    });
    *ASKED
}

/// Of the window under way: when it began, the nanoseconds spent mapping in
/// it; and whether the last one was busy.
static BUSY: (Mutex<Option<std::time::Instant>>, AtomicU64, std::sync::atomic::AtomicBool) =
    (Mutex::new(None), AtomicU64::new(0), std::sync::atomic::AtomicBool::new(false));

/// Counts the time a mapping call of the title's takes, as long as it lives.
/// More than a quarter of a window, and the calls are what loading waits for
/// (one thread of the title's makes them); less than a twentieth, they are
/// not any more.
struct Busy(std::time::Instant);

impl Busy {
    fn begin() -> Busy {
        Busy(std::time::Instant::now())
    }
}

/// Whether mapping is busy after a window of `window` with `spent` in it.
fn busy_after(was: bool, spent: std::time::Duration, window: std::time::Duration) -> bool {
    spent * if was { 20 } else { 4 } > window
}

impl Drop for Busy {
    fn drop(&mut self) {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let all = BUSY.1.fetch_add((now - self.0).as_nanos() as u64, Ordering::Relaxed) + (now - self.0).as_nanos() as u64;
        let mut began = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        let window = now - *began.get_or_insert(self.0);
        if window < WINDOW {
            return;
        }
        *began = Some(now);
        BUSY.1.store(0, Ordering::Relaxed);
        let was = BUSY.2.load(Ordering::Relaxed);
        let is = busy_after(was, std::time::Duration::from_nanos(all), window);
        if was != is {
            BUSY.2.store(is, Ordering::Relaxed);
            static SAID: AtomicU64 = AtomicU64::new(0);
            if asked_threads().is_none() && SAID.fetch_add(1, Ordering::Relaxed) < 200 {
                tracing::info!(
                    "title pages are mapped by {} (VICTORY_MAP_THREADS): {:.0} ms of the last {:.0} ms went into mapping calls",
                    if is { "several threads" } else { "the caller alone" },
                    all as f64 / 1e6,
                    window.as_secs_f64() * 1e3
                );
            }
        }
    }
}

/// A page of address space to unmap and, with a slice, map anew.
struct Op {
    at: usize,
    slice: Option<Slice>,
    /// A view of this page alone is there, to be unmapped.
    had: bool,
    /// The placeholder is the page's own (or will be, after the unmapping).
    own: bool,
    /// (`VICTORY_POOL_DISCARD`) What the view taken away showed.
    old: Option<Slice>,
    /// (V2, `VICTORY_SOFT_VIEWS`) The view taken away was in the books alone:
    /// there is nothing to unmap.
    soft: bool,
}

impl Op {
    fn new(at: usize, slice: Option<Slice>) -> Op {
        Op { at, slice, had: false, own: false, old: None, soft: false }
    }

    /// The system calls; whether the host did as asked.
    fn run(&self) -> bool {
        if self.had && !self.soft {
            // SAFETY: a view we mapped; it becomes a placeholder again.
            let gone = counted(Call::Unmap, || unsafe {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut c_void },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            });
            if gone == 0 {
                return false;
            }
        }
        let Some(slice) = &self.slice else {
            return true;
        };
        show(self.at, PAGE, slice, self.own)
    }
}

/// Does `ops`, each of another page, side by side: first what is noted of
/// the pages goes, then the system calls, then what is there now is noted.
/// With [`PHYSICAL`] and the change of mapping (`title_memory_remapping`)
/// held.
fn change(physical: &mut Physical, mut ops: Vec<Op>) -> bool {
    for op in &mut ops {
        let there = physical
            .views
            .range(..=op.at)
            .next_back()
            .filter(|(base, view)| op.at < **base + view.0)
            .map(|(base, view)| (*base, view.0));
        match there {
            Some((base, PAGE)) if base == op.at => {
                if let Some((size, slice)) = physical.views.remove(&op.at) {
                    shown_mut().forget(op.at, size, &slice);
                    op.old = Some(slice);
                }
                op.soft = soft::take(op.at) == Some(false);
                victory_gpu::title_memory_unmapping(op.at, PAGE);
                physical.holes.remove(&op.at);
                (op.had, op.own) = (true, lean());
            }
            // Part of a larger view.
            Some(_) => {
                unmap(physical, op.at, PAGE);
            }
            None => {}
        }
        if op.had {
            continue;
        }
        op.own = physical.holes.remove(&op.at) == Some(PAGE) && lean();
        if op.slice.is_some() && !op.own {
            // Cut out of a larger placeholder here, not side by side: a
            // thread that maps a piece while others cut the same one can
            // fail.
            split(op.at, PAGE);
            op.own = true;
        }
    }
    let done = pool::all(ops);
    let mut ok = true;
    for (op, done) in done {
        match op.slice {
            _ if op.had && done => {
                // (`VICTORY_UNMAP_EXACT`, `VICTORY_WINDOW_UNMAP`) The page's old
                // view is really gone (a new one may show other bytes there).
                victory_gpu::hlehooks::title_memory_unmapped(op.at, PAGE);
                if let Some(old) = &op.old {
                    pool_discard::unshown(physical, old);
                }
                if let Some(slice) = op.slice {
                    shown_mut().note(op.at, PAGE, &slice);
                    victory_gpu::title_memory_remapped(op.at, PAGE);
                    victory_gpu::title_memory_view(op.at, PAGE, slice.section.handle as usize, (slice.index * PAGE) as u64);
                    physical.views.insert(op.at, (PAGE, slice));
                } else {
                    physical.holes.insert(op.at, PAGE);
                }
            }
            Some(slice) if done => {
                // As in `map`.
                shown_mut().note(op.at, PAGE, &slice);
                victory_gpu::title_memory_remapped(op.at, PAGE);
                victory_gpu::title_memory_view(op.at, PAGE, slice.section.handle as usize, (slice.index * PAGE) as u64);
                physical.views.insert(op.at, (PAGE, slice));
            }
            None if op.had && done => {
                physical.holes.insert(op.at, PAGE);
            }
            _ => ok &= done,
        }
    }
    ok
}

/// The threads that do the system calls of [`change`].
mod pool {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    use super::{Op, side_by_side};

    struct Batch {
        ops: Vec<(Op, AtomicBool)>,
        next: AtomicUsize,
        /// Ops not done yet.
        left: AtomicUsize,
        over: (Mutex<bool>, Condvar),
    }

    impl Batch {
        /// Does ops until none is left to begin.
        fn work(&self) {
            loop {
                let i = self.next.fetch_add(1, Ordering::Relaxed);
                let Some((op, done)) = self.ops.get(i) else {
                    return;
                };
                done.store(op.run(), Ordering::Release);
                if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                    *self.over.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    self.over.1.notify_all();
                }
            }
        }
    }

    struct Pool {
        batches: Mutex<VecDeque<Arc<Batch>>>,
        more: Condvar,
    }

    fn pool() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        static STARTED: std::sync::Once = std::sync::Once::new();
        let pool = POOL.get_or_init(|| Pool { batches: Mutex::default(), more: Condvar::new() });
        STARTED.call_once(|| {
            let started = (1..super::asked_threads().unwrap_or(super::WIDE).max(side_by_side()))
                .filter(|i| std::thread::Builder::new().name(format!("mapping {i}")).spawn(|| serve(pool)).is_ok())
                .count();
            tracing::info!(started, "threads that map pages side by side (VICTORY_MAP_THREADS=1: none)");
        });
        pool
    }

    fn serve(pool: &Pool) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        // SAFETY: a plain call on our own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
        let mut batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Some(batch) = batches.front().cloned() else {
                batches = pool.more.wait(batches).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            if batch.next.load(Ordering::Relaxed) >= batch.ops.len() {
                // All begun: nobody needs to see it any more.
                if batches.front().is_some_and(|b| Arc::ptr_eq(b, &batch)) {
                    batches.pop_front();
                }
                continue;
            }
            drop(batches);
            batch.work();
            batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Runs `ops`; each with whether the host did it.
    pub(super) fn all(ops: Vec<Op>) -> Vec<(Op, bool)> {
        // Waking the others takes longer than a few quick calls.
        if ops.len() < 3 {
            return ops
                .into_iter()
                .map(|op| {
                    let done = op.run();
                    (op, done)
                })
                .collect();
        }
        let n = ops.len();
        let batch = Arc::new(Batch {
            ops: ops.into_iter().map(|op| (op, AtomicBool::new(false))).collect(),
            next: AtomicUsize::new(0),
            left: AtomicUsize::new(n),
            over: Default::default(),
        });
        let pool = pool();
        pool.batches.lock().unwrap_or_else(|e| e.into_inner()).push_back(batch.clone());
        if n > side_by_side() {
            pool.more.notify_all();
        } else {
            (1..n).for_each(|_| pool.more.notify_one());
        }
        batch.work();
        let mut over = batch.over.0.lock().unwrap_or_else(|e| e.into_inner());
        while !*over {
            over = batch.over.1.wait(over).unwrap_or_else(|e| e.into_inner());
        }
        drop(over);
        // No worker changes it any more (one may still hold it for a moment).
        batch
            .ops
            .iter()
            .map(|(op, done)| {
                let op = Op { at: op.at, slice: op.slice.clone(), had: op.had, own: op.own, old: op.old.clone(), soft: op.soft };
                (op, done.load(Ordering::Acquire))
            })
            .collect()
    }
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    let own = physical.holes.remove(&base) == Some(len) && lean();
    if !show(base, len, &slice, own) {
        return false;
    }
    // Noted before the GPU side is told: bytes DirectStorage put in earlier,
    // without notice at these addresses ([`pages_written`]), are there by
    // the time the copies of them go.
    shown_mut().note(base, len, &slice);
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    physical.views.insert(base, (len, slice));
    true
}

/// Cuts `[base, base + len)` out of the placeholder it is in (which fails
/// harmlessly when the range is the whole placeholder).
fn split(base: usize, len: usize) {
    // SAFETY: acts only on a placeholder.
    counted(Call::Split, || unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    });
}

/// The system calls of [`map`]; `own`: the placeholder at `base` is `len`
/// long already.
fn show(base: usize, len: usize, slice: &Slice, own: bool) -> bool {
    let map_view = || {
        // SAFETY: maps the section over a placeholder; acts only on one.
        counted(Call::Map, || unsafe {
            MapViewOfFile3(
                slice.section.handle,
                GetCurrentProcess(),
                base as *const c_void,
                (slice.index * PAGE) as u64,
                len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        })
    };
    let mut view = if own { map_view() } else { MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() } };
    if view.Value as usize != base {
        split(base, len);
        view = map_view();
    }
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !slice.section.commit_shown(slice.index, base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    commit_refused(len, "physical pages", std::io::Error::last_os_error());
    false
}

/// Reports memory the host would not give: at the Windows commit limit to
/// `crashes.log`, since the title usually faults soon after on the null it
/// gets; otherwise as a warning.
fn commit_refused(len: usize, what: &str, error: std::io::Error) {
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(
            len,
            what,
            &error,
            queried.then_some(&info),
        ));
    } else {
        tracing::warn!(len, %error, "committing {what} failed");
    }
}

fn commit_limit_message(
    len: usize,
    what: &str,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of {what}: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
/// (C3, `VICTORY_MAP_LOCK_FINE`) The title memory a change of `[base, base +
/// len)` takes away or maps, even for a moment: that range and every view
/// that overlaps it whole ([`unmap`] takes a view away whole and maps what
/// stays of it again).
fn affected(physical: &Physical, base: usize, len: usize) -> (usize, usize) {
    let end = base.saturating_add(len);
    let (mut lo, mut hi) = (base, end);
    if let Some((&at, &(size, _))) = physical.views.range(..=base).next_back()
        && at + size > base
    {
        lo = lo.min(at);
        hi = hi.max(at + size);
    }
    if end > base + 1 {
        for (&at, &(size, _)) in physical.views.range(base + 1..end) {
            hi = hi.max(at + size);
        }
    }
    (lo, hi)
}

/// [`affected`] of each op's page, joined where they touch.
fn affected_ops(physical: &Physical, ops: &[Op]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = ops.iter().map(|op| affected(physical, op.at, PAGE)).collect();
    out.sort_unstable();
    out.dedup_by(|next, last| {
        if next.0 > last.1 {
            return false;
        }
        last.1 = last.1.max(next.1);
        true
    });
    out
}

fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    // (`VICTORY_POOL_DISCARD`) The pages that lost a view, looked at once
    // the views are as they stay.
    let mut lost: Vec<Slice> = Vec::new();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        shown_mut().forget(at, size, &slice);
        // (V2, `VICTORY_SOFT_VIEWS`) A view in the books alone: nothing to unmap.
        let soft = soft::take(at);
        if at < base || at + size > end {
            // Parts of it stay, and are mapped again below.
            victory_gpu::title_memory_splitting(at, size);
        }
        victory_gpu::title_memory_unmapping(at, size);
        let gone = if soft == Some(false) {
            1
        } else {
            // SAFETY: a view we mapped; it becomes a placeholder again.
            counted(Call::Unmap, || unsafe {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: at as *mut c_void,
                    },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            })
        };
        if gone != 0 && at >= base && at + size <= end {
            physical.holes.insert(at, size);
        }
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, section: slice.section.clone() });
        }
        // (`VICTORY_UNMAP_EXACT`, `VICTORY_WINDOW_UNMAP`) What of the view
        // lay inside the range is really gone (the rest was mapped again).
        if gone != 0 {
            let (lo, hi) = (at.max(base), (at + size).min(end));
            if lo < hi {
                victory_gpu::hlehooks::title_memory_unmapped(lo, hi - lo);
                if pool_discard::on() {
                    lost.extend((lo..hi).step_by(PAGE).map(|a| Slice { index: slice.index + (a - at) / PAGE, section: slice.section.clone() }));
                }
            }
        }
    }
    for page in &lost {
        pool_discard::unshown(physical, page);
    }
}

/// (V2) `VICTORY_SOFT_VIEWS` (default off): views of the GPU's page-mapping
/// copies (`map_alias`, `map_aliases`: the title's `CopyPageMappingsBatchX`)
/// kept in our books alone. The title's GPU view of its streaming pool is
/// never touched by its CPU (ghidra-ledger F3), and each such view cost a
/// `MapViewOfFile3` and later an `UnmapViewOfFile2`, 100-290 µs each under
/// memory pressure behind the kernel's address-space lock: 114 of 122 slow
/// calls of a run (`docs/freeze-analysis.md`).
///
/// - `copies` (or `1`; the title's own CPU-view mapping calls are not kept
///   so in this build): such a view of one page is noted in [`Physical`],
///   [`SHOWN`] and told to the GPU side as mapped, exactly as [`map`] does,
///   over a placeholder of its own (cut once, the first time the address is
///   used), and the host maps nothing; taking it away calls nothing either.
///   Only pages committed and in a section with a lasting view
///   ([`Section::lasting`]) are kept so; others are mapped as before (counted).
///   The GPU side reads such a view from its pages ([`read`]), guards it in
///   its books, and has it made real ([`make_real`]) before anything that
///   does not fault (our writes, the kernel's, the title's questions to the
///   host about it); anything that faults (the title's CPU, a direct read of
///   ours) has it made real in its handler, and the access is made again.
/// - `verify`: every such view is mapped as before (no change at all) and
///   recorded as one that would be kept; our reads of it are compared with
///   what the books would have given (`victory_gpu` `soft_verify`), and our
///   writes and the kernel's that would have made one real are counted.
///
/// Why this cannot deadlock as `VICTORY_MAP_QUIET` did (that held our
/// protection changes, some made under the guards' lock, until a mapping
/// call was over, while the mapping call waited for the guards' lock in its
/// notes): nothing here waits for a mapping call. [`BOOKS`] is held only for
/// its own map's changes, a copy out of the lasting view and the one
/// `MapViewOfFile3` of a view made real, and nothing that holds it calls the
/// GPU side or takes another lock; the GPU side asks for the guards' lock
/// with `try_lock` only around [`make_real`], so a fault on a thread that
/// holds it (or [`PHYSICAL`], or the fine mapping lock) never waits.
/// Lock order where both are held: [`PHYSICAL`], then [`BOOKS`]; the guards,
/// then [`BOOKS`].
///
/// Line every 5 s: "soft views (VICTORY_SOFT_VIEWS=...) ...".
mod soft {
    use std::collections::{BTreeMap, HashSet};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::{LazyLock, RwLock};

    use windows_sys::Win32::System::Memory::{
        MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MapViewOfFile3, PAGE_READONLY, PAGE_READWRITE,
        VirtualFree, VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::{PAGE, Physical, Slice};

    pub(super) const OFF: u8 = 0;
    pub(super) const COPIES: u8 = 1;
    pub(super) const VERIFY: u8 = 2;

    #[cfg(test)]
    thread_local! {
        /// (Tests) The mode on this thread, beside the process's.
        pub(super) static TEST_MODE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
    }

    /// (Tests) This thread keeps views as `mode` says, the hooks set.
    #[cfg(test)]
    pub(super) fn test_mode(mode: u8) {
        static HOOKS: std::sync::Once = std::sync::Once::new();
        HOOKS.call_once(|| victory_gpu::use_soft_views(victory_gpu::SoftViews { read, make_real, verify }));
        TEST_MODE.with(|m| m.set(Some(mode)));
    }

    pub(super) fn mode() -> u8 {
        #[cfg(test)]
        if let Some(mode) = TEST_MODE.with(std::cell::Cell::get) {
            return mode;
        }
        static MODE: LazyLock<u8> = LazyLock::new(|| {
            let mode = match std::env::var("VICTORY_SOFT_VIEWS").as_deref().map(str::trim) {
                Ok("copies" | "1") => COPIES,
                Ok("verify") => VERIFY,
                _ => OFF,
            };
            if mode != OFF {
                victory_gpu::use_soft_views(victory_gpu::SoftViews { read, make_real, verify });
                tracing::info!(
                    mode = if mode == COPIES { "copies" } else { "verify" },
                    "VICTORY_SOFT_VIEWS: views of the GPU's page-mapping copies kept in the books alone until touched (verify: mapped as before, compared)"
                );
            }
            mode
        });
        *MODE
    }

    #[derive(Default)]
    struct Books {
        /// Views in the books alone, of one page each: what they show.
        soft: BTreeMap<usize, Slice>,
        /// Views that were and were made real (mapped, ours to let go as any).
        made: HashSet<usize>,
        /// (verify) Views mapped as before that would be in the books.
        would: BTreeMap<usize, Slice>,
    }

    static BOOKS: LazyLock<RwLock<Books>> = LazyLock::new(Default::default);

    fn books() -> std::sync::RwLockReadGuard<'static, Books> {
        BOOKS.read().unwrap_or_else(|e| e.into_inner())
    }

    fn books_mut() -> std::sync::RwLockWriteGuard<'static, Books> {
        BOOKS.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Since the last line (see [`line`]).
    static N: [AtomicU64; 20] = [const { AtomicU64::new(0) }; 20];
    const KEPT: usize = 0;
    const GONE_SOFT: usize = 1;
    const REAL_BY: usize = 2; // ..6: by victory_gpu::SOFT_BY_*
    const MAPS_SPARED: usize = 6;
    const UNMAPS_SPARED: usize = 7;
    const NOT_KEPT: usize = 8;
    const MAKE_FAILED: usize = 9;
    const COPY_CALLS: usize = 10;
    const COPY_NS: usize = 11;
    const COPY_LONGEST: usize = 12;
    const TITLE_CALLS: usize = 13;
    const TITLE_NS: usize = 14;
    const TITLE_LONGEST: usize = 15;
    const WOULD: usize = 16;
    const SPLITS: usize = 17;
    const GONE_REAL: usize = 18;
    /// Views in the books now (not since the last line).
    static NOW: AtomicU64 = AtomicU64::new(0);

    fn count(what: usize, n: u64) {
        N[what].fetch_add(n, Relaxed);
    }

    /// `[base, base + PAGE)` to show `slice`, kept in the books alone if it
    /// can be (`copies`): noted as [`super::map`] notes a view. Whether it was;
    /// if not, the caller maps it as before.
    pub(super) fn keep(physical: &mut Physical, base: usize, slice: &Slice) -> bool {
        if mode() != COPIES {
            return false;
        }
        if slice.section.lasting().is_none() || !slice.section.page_committed(slice.index) {
            count(NOT_KEPT, 1);
            return false;
        }
        // A placeholder of its own, as a view made real takes it at once.
        if !(physical.holes.remove(&base) == Some(PAGE) && super::lean()) {
            super::split(base, PAGE);
            count(SPLITS, 1);
        }
        // Known as such on the GPU side before it hears of the view.
        victory_gpu::soft_view_noted(base, PAGE);
        books_mut().soft.insert(base, slice.clone());
        NOW.fetch_add(1, Relaxed);
        super::shown_mut().note(base, PAGE, slice);
        victory_gpu::title_memory_remapped(base, PAGE);
        victory_gpu::title_memory_view(base, PAGE, slice.section.handle as usize, (slice.index * PAGE) as u64);
        physical.views.insert(base, (PAGE, slice.clone()));
        count(KEPT, 1);
        count(MAPS_SPARED, 1);
        true
    }

    /// (verify) `[base, base + PAGE)` was mapped as before and would have been kept.
    pub(super) fn would(base: usize, slice: &Slice) {
        if mode() != VERIFY || slice.section.lasting().is_none() || !slice.section.page_committed(slice.index) {
            return;
        }
        books_mut().would.insert(base, slice.clone());
        victory_gpu::soft_view_would(base, PAGE, true);
        count(WOULD, 1);
    }

    /// The view at `at` is being taken away: `Some(false)` if it was in the
    /// books alone (nothing to unmap: its placeholder is its own), `Some(true)`
    /// if it was and was made real (or, verify, would have been), `None` if it
    /// is none of ours here.
    pub(super) fn take(at: usize) -> Option<bool> {
        if mode() == OFF {
            return None;
        }
        let mut b = books_mut();
        if b.soft.remove(&at).is_some() {
            drop(b);
            NOW.fetch_sub(1, Relaxed);
            victory_gpu::soft_view_gone(at, PAGE);
            count(GONE_SOFT, 1);
            count(UNMAPS_SPARED, 1);
            return Some(false);
        }
        if b.made.remove(&at) {
            drop(b);
            victory_gpu::soft_view_gone(at, PAGE);
            count(GONE_REAL, 1);
            return Some(true);
        }
        if b.would.remove(&at).is_some() {
            drop(b);
            victory_gpu::soft_view_would(at, PAGE, false);
            return Some(true);
        }
        None
    }

    /// [`victory_gpu::SoftViews::read`].
    pub(super) fn read(at: usize, to: *mut u8, len: usize) -> (bool, usize) {
        let b = books();
        if let Some((&base, slice)) = b.soft.range(..=at).next_back()
            && at < base + PAGE
        {
            let n = (base + PAGE - at).min(len);
            if let Some(window) = slice.section.lasting() {
                // SAFETY: committed bytes of the section's lasting view, which the
                // slice held in the books keeps; `to` has room for `len` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping((window + slice.index * PAGE + (at - base)) as *const u8, to, n);
                }
                return (true, n);
            }
        }
        let next = b.soft.range(at.saturating_add(1)..).next().map(|(&base, _)| base);
        (false, next.map_or(len, |next| (next - at).min(len)))
    }

    /// [`victory_gpu::SoftViews::make_real`]. Never logs (it may run in the
    /// fault handler): failures are counted for [`line`].
    fn make_real(at: usize, len: usize, read_only: &dyn Fn(usize) -> bool, by: u8) -> bool {
        let end = at.saturating_add(len.max(1));
        let mut b = books_mut();
        let hits: Vec<usize> =
            b.soft.range(..end).rev().take_while(|(base, _)| **base + PAGE > at).map(|(&base, _)| base).collect();
        let mut any = false;
        for base in hits {
            let Some(slice) = b.soft.get(&base).cloned() else { continue };
            // Mapped read-write and then made read-only where asked: a view
            // mapped read-only could never be opened again.
            let read_only = read_only(base / PAGE);
            let map = || {
                // SAFETY: the section over the view's own placeholder (made
                // when it was kept); acts only on a placeholder.
                unsafe {
                    MapViewOfFile3(
                        slice.section.handle,
                        GetCurrentProcess(),
                        base as *const c_void,
                        (slice.index * PAGE) as u64,
                        PAGE,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                }
                .Value as usize
                    == base
            };
            // (Not its own placeholder after all: cut out, once more.)
            // SAFETY: acts only on a placeholder; fails harmlessly on one that
            // is the page's own already.
            if !map()
                && (unsafe { VirtualFree(base as *mut c_void, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 || !map())
            {
                count(MAKE_FAILED, 1);
                continue;
            }
            if read_only {
                let mut old = 0;
                // SAFETY: the view just mapped; only its protection changes.
                if unsafe { VirtualProtect(base as *const c_void, PAGE, PAGE_READONLY, &mut old) } == 0 {
                    count(MAKE_FAILED, 1);
                }
            }
            b.soft.remove(&base);
            b.made.insert(base);
            NOW.fetch_sub(1, Relaxed);
            victory_gpu::soft_view_real(base, PAGE);
            count(REAL_BY + usize::from(by.min(3)), 1);
            any = true;
        }
        any
    }

    /// [`victory_gpu::SoftViews::verify`]: each view of `[at, at + len)` that
    /// would be in the books, its page as mapped against the lasting view's.
    pub(super) fn verify(at: usize, len: usize) -> (u64, u64) {
        let end = at.saturating_add(len);
        let b = books();
        let (mut same, mut differ) = (0, 0);
        for (&base, slice) in b.would.range(at.saturating_sub(PAGE - 1)..end) {
            if base + PAGE <= at {
                continue;
            }
            let Some(window) = slice.section.lasting() else { continue };
            // SAFETY: a view mapped for real (its unmapping waits for the
            // books: `take`), and the same page in the section's lasting view.
            let (here, there) = unsafe {
                (
                    std::slice::from_raw_parts(base as *const u8, PAGE),
                    std::slice::from_raw_parts((window + slice.index * PAGE) as *const u8, PAGE),
                )
            };
            if here == there {
                same += 1;
            } else {
                differ += 1;
                static SAID: AtomicU64 = AtomicU64::new(0);
                if SAID.fetch_add(1, Relaxed) < 20 {
                    let k = here.iter().zip(there).position(|(a, b)| a != b).unwrap_or(0);
                    tracing::warn!(
                        view = format_args!("{base:#x}"),
                        section = format_args!("{:#x}", slice.section.handle as usize),
                        page = slice.index,
                        first_byte = k,
                        "soft views (VICTORY_SOFT_VIEWS=verify): a view's bytes differ from what the books read (or changed between the two reads)"
                    );
                }
            }
        }
        (same, differ)
    }

    /// The title asks the host about `[at, at + len)` (a question, an
    /// allocation at an address): views in the books there are made real first.
    pub(super) fn title_asks(at: usize, len: usize) {
        if mode() == COPIES {
            victory_gpu::soft_make_real(at, len, victory_gpu::SOFT_BY_TITLE_CALL);
        }
    }

    thread_local! {
        static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// Times a mapping call, the outermost on its thread, while it lives
    /// (counted with the switch set, `verify` too: the two runs compare).
    pub(super) struct Timed(Option<(std::time::Instant, usize)>);

    impl Timed {
        pub(super) fn copies() -> Timed {
            Timed::begin(COPY_CALLS)
        }

        pub(super) fn title() -> Timed {
            Timed::begin(TITLE_CALLS)
        }

        fn begin(kind: usize) -> Timed {
            if mode() == OFF {
                return Timed(None);
            }
            let outer = DEPTH.with(|d| {
                d.set(d.get() + 1);
                d.get() == 1
            });
            Timed(outer.then(|| (std::time::Instant::now(), kind)))
        }
    }

    impl Drop for Timed {
        fn drop(&mut self) {
            if mode() == OFF {
                return;
            }
            DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            let Some((began, kind)) = self.0 else { return };
            let ns = began.elapsed().as_nanos() as u64;
            count(kind, 1);
            count(kind + 1, ns);
            N[kind + 2].fetch_max(ns, Relaxed);
            line();
        }
    }

    /// The line, every five seconds.
    fn line() {
        static LAST: AtomicU64 = AtomicU64::new(0);
        static START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
        let now = START.elapsed().as_millis() as u64;
        let last = LAST.load(Relaxed);
        if now < last + 5000 || LAST.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
            return;
        }
        let n: Vec<u64> = N.iter().map(|n| n.swap(0, Relaxed)).collect();
        let g = victory_gpu::soft_view_counts();
        let ms = |ns: u64| ns as f64 / 1e6;
        tracing::info!(
            "soft views (VICTORY_SOFT_VIEWS={}), last {:.1} s: kept soft {} (in the books now {}), let go untouched {}, let go after made real {}; \
             made real by a title fault {}, by a fault of other code {}, by our writes {}, by title calls {}; \
             map calls spared {}, unmap calls spared {}, placeholders cut {}, not kept (uncommitted or no lasting view) {}, made real FAILED {}; \
             time in the mapping calls: copies {} calls {:.1} ms (longest {:.1} ms), title's own {} calls {:.1} ms (longest {:.1} ms); \
             reads from the books {} ({} KiB); guarded in the books {} here, {} as another view; made real read-only for want of the guards' lock {}, \
             writes let through to such {}; verify: views that would be kept {}, our reads of them {} ({} KiB), pages compared the same {}, DIFFERING {}, \
             our writes and the kernel's that would make one real {}",
            match mode() {
                COPIES => "copies",
                _ => "verify",
            },
            (now - last) as f64 / 1e3,
            n[KEPT],
            NOW.load(Relaxed),
            n[GONE_SOFT],
            n[GONE_REAL],
            n[REAL_BY],
            n[REAL_BY + 1],
            n[REAL_BY + 2],
            n[REAL_BY + 3],
            n[MAPS_SPARED],
            n[UNMAPS_SPARED],
            n[SPLITS],
            n[NOT_KEPT],
            n[MAKE_FAILED],
            n[COPY_CALLS],
            ms(n[COPY_NS]),
            ms(n[COPY_LONGEST]),
            n[TITLE_CALLS],
            ms(n[TITLE_NS]),
            ms(n[TITLE_LONGEST]),
            g[0],
            g[1] >> 10,
            g[2],
            g[3],
            g[4],
            g[5],
            n[WOULD],
            g[6],
            g[7] >> 10,
            g[8],
            g[9],
            g[10]
        );
    }

    #[cfg(test)]
    pub(super) fn soft_now(at: usize) -> bool {
        books().soft.contains_key(&at)
    }

    #[cfg(test)]
    pub(super) fn made_now(at: usize) -> bool {
        books().made.contains(&at)
    }

}

/// (Storage wait job, ported) `VICTORY_POOL_DISCARD` (fox port: default 0,
/// the fox base's behaviour, until an A/B run; 1 on, our tree's default;
/// needs the section's lasting view, `VICTORY_SECTION_WINDOW`, the fox
/// base's form of our `VICTORY_DSTORAGE_LINEAR_VIEW`). The lasting view
/// keeps every page DirectStorage ever wrote through it in this process's
/// working set, also after the title has given the page back (run-49: 24.7
/// GB working set against 19.1). The title's streaming pool is one
/// allocation it shows whole at one address for good (its pool view) and
/// maps page by page into its CPU and GPU streaming views from a free list;
/// a page shown by no view but a whole-section one is on that free list and
/// its bytes are dead. Such a page, still so [`pool_discard::delay`] after it was
/// last unmapped and while free RAM is under `VICTORY_POOL_DISCARD_BELOW_MB`
/// (4096; 0: always), is reset in the lasting view (`MEM_RESET`) and taken
/// out of the working set (`VirtualUnlock`), under the views' lock, so no
/// map of it can come between the check and the reset. Pages of sections
/// without a whole view are never touched. `VICTORY_POOL_DISCARD_DELAY_S`:
/// 0 (default) adaptive 2-10 s, N fixed. Line every 30 s: "pool discard: ...".
mod pool_discard {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::{Arc, LazyLock, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use super::{PAGE, Physical, Section, Slice};

    pub(super) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let window = std::env::var_os("VICTORY_SECTION_WINDOW").is_none_or(|v| v != "0");
            let on = window && std::env::var_os("VICTORY_POOL_DISCARD").is_some_and(|v| v == "1");
            tracing::info!(on, "VICTORY_POOL_DISCARD: pool pages the title has given back (shown by no view but its whole pool view) are reset and taken out of the working set in the section's lasting view while free RAM is short");
            on
        })
    }

    /// Pages resident through the lasting views, discarded since the
    /// start, resets refused, rewritten within 5 s of their discard.
    static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    const RESIDENT: usize = 0;
    const DISCARDED: usize = 1;
    const FAILED: usize = 2;
    const REWRITTEN: usize = 3;

    /// Pages that lost a view, to look at again later: when, and the page.
    static UNSHOWN: Mutex<VecDeque<(Instant, Slice)>> = Mutex::new(VecDeque::new());
    /// The most pages kept noted.
    const UNSHOWN_MOST: usize = 1 << 18;
    /// When each page was discarded, until written again (or 5 s on).
    static DISCARDED_AT: LazyLock<Mutex<HashMap<(usize, usize), Instant>>> = LazyLock::new(Default::default);
    /// Discards in the adaptive window, and of them rewritten within 5 s.
    static WINDOW: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    static HELD_BY_RAM: AtomicBool = AtomicBool::new(false);
    static DELAY_MS: AtomicU64 = AtomicU64::new(2000);

    fn key(section: &Section, index: usize) -> (usize, usize) {
        (std::ptr::from_ref(section) as usize, index)
    }

    /// Pages `[index, index + n)` of `section` were written through its
    /// lasting view: resident there from now on.
    pub(super) fn written(section: &Section, index: usize, n: usize) {
        if !on() {
            return;
        }
        for k in index..(index + n).min(section.pages) {
            let bit = 1u64 << (k % 64);
            if section.resident[k / 64].fetch_or(bit, Relaxed) & bit == 0 {
                COUNTS[RESIDENT].fetch_add(1, Relaxed);
            }
            if section.discarded[k / 64].fetch_and(!bit, Relaxed) & bit != 0 {
                let at = DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).remove(&key(section, k));
                if at.is_some_and(|at| at.elapsed() < Duration::from_secs(5)) {
                    COUNTS[REWRITTEN].fetch_add(1, Relaxed);
                    WINDOW[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether only whole-section views show `page` now, and at least one.
    fn only_whole_views(physical: &Physical, page: &Slice) -> bool {
        let addresses = super::SHOWN.read().unwrap_or_else(|e| e.into_inner()).at(page);
        let size = page.section.pages * PAGE;
        !addresses.is_empty()
            && addresses.iter().all(|&(a, _)| {
                physical
                    .views
                    .range(..=a)
                    .next_back()
                    .is_some_and(|(&at, (len, slice))| at + len > a && *len == size && slice.index == 0)
            })
    }

    /// A page lost a view (with the views' lock held): noted if it is
    /// resident in our lasting view and only the pool view shows it now.
    pub(super) fn unshown(physical: &Physical, page: &Slice) {
        if !on() {
            return;
        }
        let bit = 1u64 << (page.index % 64);
        if page.index >= page.section.pages
            || page.section.resident[page.index / 64].load(Relaxed) & bit == 0
            || !only_whole_views(physical, page)
        {
            return;
        }
        UNSHOWN.lock().unwrap_or_else(|e| e.into_inner()).push_back((Instant::now(), page.clone()));
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let _ = std::thread::Builder::new().name("victory-pool-discard".into()).spawn(discard_loop);
        });
    }

    fn below() -> u64 {
        static MB: OnceLock<u64> = OnceLock::new();
        let mb = *MB.get_or_init(|| {
            let mb = std::env::var("VICTORY_POOL_DISCARD_BELOW_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096);
            tracing::info!(mb, "VICTORY_POOL_DISCARD_BELOW_MB: pool pages are discarded only while free RAM is under this (0: always)");
            mb
        });
        if mb == 0 { u64::MAX } else { mb << 20 }
    }

    fn free_ram() -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data the call fills in.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: a sized structure.
        (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullAvailPhys)
    }

    fn delay_setting() -> u64 {
        static S: OnceLock<u64> = OnceLock::new();
        *S.get_or_init(|| {
            let s = std::env::var("VICTORY_POOL_DISCARD_DELAY_S").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
            tracing::info!(s, "VICTORY_POOL_DISCARD_DELAY_S: wait after a pool page's last unmapping before it is discarded (0: adaptive 2-10 s by pages rewritten within 5 s)");
            s
        })
    }

    /// The next adaptive delay from the last window's discards `d` and of
    /// them rewritten within 5 s `r`, and free RAM: more than a quarter
    /// rewritten doubles it (up to 10 s), under 5 % halves it (down to 2 s);
    /// under 1 GiB free RAM it is 2 s.
    pub(super) fn next_delay(ms: u64, d: u64, r: u64, free: Option<u64>) -> u64 {
        if free.is_some_and(|f| f < 1 << 30) {
            return 2000;
        }
        if d >= 64 && r * 4 > d {
            (ms * 2).min(10_000)
        } else if d >= 64 && r * 20 < d {
            (ms / 2).max(2000)
        } else {
            ms
        }
    }

    fn delay() -> Duration {
        match delay_setting() {
            0 => Duration::from_millis(DELAY_MS.load(Relaxed)),
            s => Duration::from_secs(s),
        }
    }

    fn discard_loop() {
        use windows_sys::Win32::System::Memory::{MEM_RESET, PAGE_READWRITE, VirtualAlloc, VirtualUnlock};
        let mut window = Instant::now();
        let mut said = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if window.elapsed() >= Duration::from_secs(5) {
                window = Instant::now();
                let (d, r) = (WINDOW[0].swap(0, Relaxed), WINDOW[1].swap(0, Relaxed));
                DELAY_MS.store(next_delay(DELAY_MS.load(Relaxed), d, r, free_ram()), Relaxed);
                DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, at| at.elapsed() < Duration::from_secs(5));
            }
            if said.elapsed() >= Duration::from_secs(30) {
                said = Instant::now();
                tracing::info!("{}", line());
            }
            let quiet = delay();
            if free_ram().is_some_and(|free| free >= below()) {
                HELD_BY_RAM.store(true, Relaxed);
                let mut queue = UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
                let over = queue.len().saturating_sub(UNSHOWN_MOST);
                queue.drain(..over);
                continue;
            }
            HELD_BY_RAM.store(false, Relaxed);
            let due: Vec<Slice> = {
                let mut queue = UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
                let n = queue.iter().take_while(|(at, _)| at.elapsed() >= quiet).count();
                queue.drain(..n).map(|(_, s)| s).collect()
            };
            // Pieces of 1,024 pages, the lock let go between them.
            for piece in due.chunks(1024) {
                let physical = super::physical();
                // (lasting view base, first index, pages)
                let mut runs: Vec<(usize, usize, usize, Arc<Section>)> = Vec::new();
                for page in piece {
                    let section = &*page.section;
                    let Some(&base) = section.window.get() else { continue };
                    let bit = 1u64 << (page.index % 64);
                    if base == 0 || section.resident[page.index / 64].load(Relaxed) & bit == 0 || !only_whole_views(&physical, page) {
                        continue;
                    }
                    section.resident[page.index / 64].fetch_and(!bit, Relaxed);
                    section.discarded[page.index / 64].fetch_or(bit, Relaxed);
                    DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).insert(key(section, page.index), Instant::now());
                    WINDOW[0].fetch_add(1, Relaxed);
                    match runs.last_mut() {
                        Some((b, first, n, _)) if *b == base && *first + *n == page.index => *n += 1,
                        _ => runs.push((base, page.index, 1, page.section.clone())),
                    }
                }
                for (base, first, n, _section) in runs {
                    let (at, len) = ((base + first * PAGE) as *mut core::ffi::c_void, n * PAGE);
                    // SAFETY: committed pages of our lasting view whose bytes
                    // are dead (above); the lock keeps maps of them out, and
                    // `_section` keeps the view.
                    let reset = unsafe { VirtualAlloc(at, len, MEM_RESET, PAGE_READWRITE) };
                    if reset.is_null() && COUNTS[FAILED].fetch_add(1, Relaxed) == 0 {
                        tracing::warn!(error = %std::io::Error::last_os_error(), "VICTORY_POOL_DISCARD: MEM_RESET refused (taken out of the working set only)");
                    }
                    // SAFETY: as above; on pages not locked this only takes
                    // them out of the working set.
                    unsafe { VirtualUnlock(at, len) };
                    COUNTS[RESIDENT].fetch_sub(n as u64, Relaxed);
                    COUNTS[DISCARDED].fetch_add(n as u64, Relaxed);
                }
                drop(physical);
            }
        }
    }

    /// "pool discard: ..." with the process's working set.
    fn line() -> String {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        let mb = |pages: u64| (pages * PAGE as u64) >> 20;
        let failed = COUNTS[FAILED].load(Relaxed);
        format!(
            "pool discard: pool pages resident {} MB, discarded this run {} MB, rewritten within 5 s of a discard {} MB (delay {} ms), working set {} MB (peak {} MB) (VICTORY_POOL_DISCARD=1{}; only under {} MB free RAM, VICTORY_POOL_DISCARD_BELOW_MB{})",
            mb(COUNTS[RESIDENT].load(Relaxed)),
            mb(COUNTS[DISCARDED].load(Relaxed)),
            mb(COUNTS[REWRITTEN].load(Relaxed)),
            delay().as_millis(),
            c.WorkingSetSize >> 20,
            c.PeakWorkingSetSize >> 20,
            if failed == 0 { String::new() } else { format!(", resets refused {failed}") },
            if below() == u64::MAX { "any".to_owned() } else { (below() >> 20).to_string() },
            if HELD_BY_RAM.load(Relaxed) { "; held now: RAM plentiful" } else { "" },
        )
    }

    #[cfg(test)]
    mod tests {
        use super::next_delay;

        #[test]
        fn the_discard_delay_adapts_to_pages_rewritten_soon() {
            let plenty = Some(8u64 << 30);
            assert_eq!(next_delay(2000, 100, 30, plenty), 4000);
            assert_eq!(next_delay(8000, 100, 30, plenty), 10_000);
            assert_eq!(next_delay(4000, 100, 2, plenty), 2000);
            assert_eq!(next_delay(4000, 100, 10, plenty), 4000);
            assert_eq!(next_delay(4000, 10, 10, plenty), 4000, "too few discards to judge");
            assert_eq!(next_delay(8000, 100, 30, Some(512 << 20)), 2000, "short of RAM");
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: if title_used_is_process() {
            counters.PagefileUsage
        } else {
            TITLE_PAGES.load(Ordering::Relaxed)
        },
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

/// The title's own `VirtualFree`. What the GPU side noted of the memory goes
/// first: the address may be other memory's the moment it is free.
extern "system" fn title_virtual_free(address: *mut c_void, size: usize, kind: u32) -> BOOL {
    const MEM_COALESCE_PLACEHOLDERS: u32 = 1;
    // Splitting and joining placeholders changes nothing they show.
    if kind & (MEM_PRESERVE_PLACEHOLDER | MEM_COALESCE_PLACEHOLDERS) == 0 {
        victory_gpu::title_memory_freed(address as usize, size);
    }
    // SAFETY: the title's own arguments.
    unsafe { VirtualFree(address, size, kind) }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll", "kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
        exports: vec![
            crate::HleExport { name: "VirtualFree", address: title_virtual_free as *const () as usize },
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (V2) The host's answer alone (the title's question makes views in the books real).
    fn host_state(address: *const c_void) -> u32 {
        // SAFETY: plain data; zero is a valid value.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        assert_ne!(unsafe { windows_sys::Win32::System::Memory::VirtualQuery(address, &mut info, size_of_val(&info)) }, 0);
        info.State
    }

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
        assert!(report.contains("4194304 bytes of physical pages"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
        assert!(fallback.contains("4194304 bytes of virtual memory"));
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// (V2, `VICTORY_SOFT_VIEWS=copies`) A page-mapping copy's view kept in
    /// the books alone: the host has a placeholder there and maps nothing; the
    /// books read its page's bytes; a touch makes it real (the access made
    /// again), and so does a question of the title's; let go untouched it
    /// leaves a placeholder of its own that a view of the title's takes.
    #[test]
    fn views_of_the_copies_are_kept_in_the_books_alone() {
        soft::test_mode(soft::COPIES);
        let mut pages = [0usize; 2];
        let mut n = 2;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_ptr().cast_mut()), 1);
        let reserve = |pages: usize| XMemVirtualAlloc(std::ptr::null_mut(), pages * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let (pool, gpu) = (reserve(2), reserve(3));
        assert!(!pool.is_null() && !gpu.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(pool, 2, pages.as_ptr()), pool);
        // SAFETY: mapped read-write.
        unsafe {
            pool.cast::<u8>().add(5).write(0x55);
            at(pool, 1).cast::<u8>().add(7).write(0x77);
        }
        // Two pages at the GPU view, the middle one of three.
        assert!(map_aliases(&[(at(gpu, 1) as u64, pool as u64, PAGE as u64), (at(gpu, 2) as u64, at(pool, 1) as u64, PAGE as u64)]));
        for k in [1, 2] {
            assert!(soft::soft_now(at(gpu, k) as usize), "page {k} in the books");
            assert_eq!(host_state(at(gpu, k)), MEM_RESERVE, "page {k}: nothing mapped");
        }
        // The books read what the pages hold, from inside one view to the next.
        let mut out = vec![0u8; PAGE];
        let from = at(gpu, 1) as usize + PAGE / 2;
        assert_eq!(soft::read(from, out.as_mut_ptr(), PAGE), (true, PAGE / 2));
        assert_eq!(soft::read(from + PAGE / 2, out[PAGE / 2..].as_mut_ptr(), PAGE / 2), (true, PAGE / 2));
        assert_eq!(out[PAGE / 2 + 7], 0x77);
        // Before the views: no view there, up to the first.
        assert_eq!(soft::read(gpu as usize, out.as_mut_ptr(), 2 * PAGE), (false, PAGE));
        // The page shown as the title would see it, in our records.
        assert!(pages_shown(&pages[..1], 0, PAGE).contains(&(at(gpu, 1) as usize, PAGE)));
        // A touch: made real (the GPU side's handler) and read again.
        // SAFETY: the view in the books is made real by the read's fault.
        assert_eq!(unsafe { at(gpu, 1).cast::<u8>().add(5).read_volatile() }, 0x55);
        assert!(soft::made_now(at(gpu, 1) as usize) && !soft::soft_now(at(gpu, 1) as usize));
        assert_eq!(host_state(at(gpu, 1)), MEM_COMMIT);
        // Read-write: nothing guards it.
        // SAFETY: mapped read-write now.
        unsafe { at(gpu, 1).cast::<u8>().add(6).write_volatile(0x66) };
        // SAFETY: the same page in the pool view.
        assert_eq!(unsafe { pool.cast::<u8>().add(6).read() }, 0x66);
        // A question of the title's makes the other real first.
        // SAFETY: a pseudo handle of our own process.
        assert_ne!(XMemVirtualQuery(unsafe { GetCurrentProcess() }, at(gpu, 2), &mut query(gpu)), 0);
        assert!(soft::made_now(at(gpu, 2) as usize));
        assert_eq!(host_state(at(gpu, 2)), MEM_COMMIT);
        // Let go (made real: unmapped as any view).
        assert!(map_aliases(&[(at(gpu, 1) as u64, 0, 2 * PAGE as u64)]));
        for k in [1, 2] {
            assert!(!soft::made_now(at(gpu, k) as usize) && !soft::soft_now(at(gpu, k) as usize));
            assert_eq!(host_state(at(gpu, k)), MEM_RESERVE);
        }
        // Kept again, and let go untouched: no call, and the placeholder
        // left is the page's own, which a view of the title's takes.
        assert!(map_alias(gpu as u64, at(pool, 1) as u64, PAGE as u64));
        assert!(soft::soft_now(gpu as usize));
        assert!(map_alias(gpu as u64, 0, PAGE as u64));
        assert!(!soft::soft_now(gpu as usize));
        assert_eq!(XMemMapPhysicalPages(gpu, 1, pages[1..].as_ptr()), gpu);
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { gpu.cast::<u8>().add(7).read() }, 0x77);
        // A view of the title's over one in the books takes its place.
        assert!(map_alias(at(gpu, 1) as u64, pool as u64, PAGE as u64));
        assert!(soft::soft_now(at(gpu, 1) as usize));
        assert_eq!(XMemMapPhysicalPages(at(gpu, 1), 1, pages[..1].as_ptr()), at(gpu, 1));
        assert!(!soft::soft_now(at(gpu, 1) as usize));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(gpu, 1).cast::<u8>().add(5).read() }, 0x55);
        assert_eq!(XMemMapPhysicalPages(gpu, 2, std::ptr::null()), gpu);
        assert_eq!(XMemMapPhysicalPages(pool, 2, std::ptr::null()), pool);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        soft::test_mode(soft::OFF);
    }

    /// (V2, `VICTORY_SOFT_VIEWS=verify`) The copies' views mapped as before,
    /// recorded as views that would be kept: the books read the same bytes.
    #[test]
    fn verify_maps_as_before_and_the_books_read_the_same() {
        soft::test_mode(soft::VERIFY);
        let mut pages = [0usize; 1];
        let mut n = 1;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_ptr().cast_mut()), 1);
        let reserve = || XMemVirtualAlloc(std::ptr::null_mut(), PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let (pool, gpu) = (reserve(), reserve());
        assert_eq!(XMemMapPhysicalPages(pool, 1, pages.as_ptr()), pool);
        // SAFETY: mapped read-write.
        unsafe { pool.cast::<u8>().add(9).write(0x99) };
        assert!(map_alias(gpu as u64, pool as u64, PAGE as u64));
        assert_eq!(query(gpu).host.State, MEM_COMMIT, "mapped as before");
        assert!(!soft::soft_now(gpu as usize));
        assert_eq!(soft::verify(gpu as usize, PAGE), (1, 0), "{}", describe(gpu as usize));
        assert!(map_alias(gpu as u64, 0, PAGE as u64));
        assert_eq!(soft::verify(gpu as usize, PAGE), (0, 0), "forgotten with the view");
        assert_eq!(XMemMapPhysicalPages(pool, 1, std::ptr::null()), pool);
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
        soft::test_mode(soft::OFF);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                // (Once at least: the mapping thread may be done before this
                // one's first turn.)
                while reads == 0 || !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A title thread that reads and writes a page of a view while other
    /// pages of the view are unmapped and mapped again, as the console lets
    /// it: the page was never given up.
    #[test]
    fn a_title_thread_goes_on_using_the_rest_of_a_view() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let pages = allocate(4);
        for _ in 0..200 {
            // A new reservation each time: the pieces of a view cut up stay
            // cut up.
            let a = XMemVirtualAlloc(
                std::ptr::null_mut(),
                4 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            );
            assert!(!a.is_null());
            assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
            let (first, stop) = (a as usize, AtomicBool::new(false));
            // SAFETY: mapped read-write.
            unsafe { (first as *mut u64).write_volatile(0) };
            std::thread::scope(|s| {
                let title = s.spawn(|| {
                    let mut n = 0u64;
                    // SAFETY (the block): the first page is the title's all
                    // the time; a fault would end the process.
                    unsafe {
                        while !stop.load(Ordering::Relaxed) {
                            assert_eq!((first as *const u64).read_volatile(), n);
                            n += 1;
                            (first as *mut u64).write_volatile(n);
                        }
                    }
                    n
                });
                // SAFETY: as above.
                while !title.is_finished() && unsafe { (first as *const u64).read_volatile() } < 100 {}
                for page in [3, 2, 1] {
                    let at = a.wrapping_byte_add(page * PAGE);
                    assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null()), at);
                    assert_eq!(XMemMapPhysicalPages(at, 1, pages[page..].as_ptr()), at);
                }
                stop.store(true, Ordering::Relaxed);
                assert!(title.join().expect("the title's thread never faulted") > 0);
            });
            assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        }
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Many pages in one call: mapped, moved over each other's addresses,
    /// given twice in one list, copied by the GPU's mappings and unmapped.
    #[test]
    fn a_scatter_of_many_pages_is_what_one_after_the_other_is() {
        const N: usize = 200;
        let pages = allocate(N);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 2 * N * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
        let at = |k: usize| (a + k * PAGE) as *mut c_void;
        let first = |k: usize| at(k).cast::<u64>();
        for round in 0..4_usize {
            // Every address shows another page each round.
            let to: Vec<*mut c_void> = (0..N).map(|i| at((i + 7 * round) % N)).collect();
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N, pages.as_ptr()), 1);
            for i in 0..N {
                // SAFETY: mapped read-write.
                unsafe {
                    if round > 0 {
                        assert_eq!(first((i + 7 * round) % N).read(), (1000 * (round - 1) + i) as u64);
                    }
                    first((i + 7 * round) % N).write((1000 * round + i) as u64);
                }
            }
        }
        // The GPU's copies of the mappings, in ranges and one by one; a page
        // mapped from one the same call has mapped shows the same.
        let half = (N / 2 * PAGE) as u64;
        let (from, to) = (a as u64, at(N) as u64);
        assert!(map_aliases(&[(to, from, half), (to + half, to, half)]));
        for k in 0..N / 2 {
            // SAFETY: mapped read-write.
            unsafe {
                assert_eq!(first(N + k).read(), first(k).read());
                assert_eq!(first(N + N / 2 + k).read(), first(k).read());
                first(N + k).write(5);
                assert_eq!(first(k).read(), 5);
            }
        }
        // An address twice: the last one counts; page 0 unmaps.
        let to = [at(0), at(1), at(0), at(2), at(3), at(0)];
        let what = [pages[5], pages[6], 0, 0, pages[7], pages[8]];
        assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 6, what.as_ptr()), 1);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        // SAFETY: mapped read-write; the same page at both.
        unsafe {
            first(0).write(77);
            assert_eq!(XMemMapPhysicalPagesScatter([at(2), at(4), at(6)].as_ptr(), 3, [pages[8]; 3].as_ptr()), 1);
            assert_eq!((first(2).read(), first(4).read(), first(6).read()), (77, 77, 77));
        }
        // A page the title does not own.
        assert_eq!(XMemMapPhysicalPagesScatter([at(0), at(1)].as_ptr(), 2, [pages[0], 1].as_ptr()), 0);
        // All gone, also parts of a larger view.
        assert_eq!(XMemMapPhysicalPages(at(0), 4, std::ptr::null()), at(0));
        // SAFETY: placeholders of ours, nothing mapped in them.
        unsafe { VirtualFree(at(0), 4 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */) };
        assert_eq!(XMemMapPhysicalPages(at(0), 4, pages.as_ptr()), at(0));
        assert!(map_aliases(&[(a as u64, 0, (2 * N * PAGE) as u64)]));
        assert!((0..2 * N).all(|k| query(at(k)).host.State == MEM_RESERVE));
        assert!(physical().views.range(a..a + 2 * N * PAGE).next().is_none());
        assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
    }

    /// Pages keep what they hold however often they are mapped, moved and
    /// unmapped without a commit or a split of their own.
    #[test]
    fn pages_mapped_again_hold_what_they_held() {
        let pages = allocate(3);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 8 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        for round in 0..6_usize {
            for (k, page) in pages.iter().enumerate() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, page), to);
                let first = to.cast::<u64>();
                // SAFETY: mapped read-write; the far end of the page too.
                unsafe {
                    if round > 0 {
                        assert_eq!(first.read(), (round * 16 + k - 16) as u64);
                        assert_eq!(first.add(PAGE / 8 - 1).read(), k as u64);
                    }
                    first.write((round * 16 + k) as u64);
                    first.add(PAGE / 8 - 1).write(k as u64);
                }
            }
            for k in 0..pages.len() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, std::ptr::null()), to);
                assert_eq!(query(to).host.State, MEM_RESERVE);
            }
            // The title joins what it unmapped: the next view splits it anew.
            if round == 3 {
                // SAFETY: placeholders of ours, nothing mapped in them.
                unsafe {
                    VirtualFree(a, 8 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */)
                };
            }
        }
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// What a title that maps and unmaps pages all the time costs the
    /// workers that write others: 24 threads look pages up, fill one and
    /// tell of it, as a request does, while one maps.
    /// `cargo test --release -p victory-hle --lib map_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn map_bench() {
        use std::sync::atomic::AtomicU64;
        use std::time::{Duration, Instant};
        let pages = allocate(1024);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 1024 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
        for mapping in [false, true] {
            let (done, waited, worst, maps) =
                (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
            let until = Instant::now() + Duration::from_secs(3);
            std::thread::scope(|s| {
                if mapping {
                    s.spawn(|| {
                        let at: Vec<*mut c_void> = (0..256).map(|i| (a + i * PAGE) as *mut c_void).collect();
                        while Instant::now() < until {
                            assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, pages.as_ptr()), 1);
                            assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, std::ptr::null()), 1);
                            maps.fetch_add(512, Ordering::Relaxed);
                        }
                    });
                }
                for t in 0..24 {
                    let (pages, done, waited, worst) = (&pages, &done, &waited, &worst);
                    s.spawn(move || {
                        let mut i = 256 + t * 16;
                        while Instant::now() < until {
                            let page = &pages[i..=i];
                            i = if i == 256 + t * 16 + 15 { 256 + t * 16 } else { i + 1 };
                            let began = Instant::now();
                            assert!(pages_known(page, 0, PAGE));
                            let mut window = page_window(page, 0, PAGE).unwrap();
                            let looked = began.elapsed();
                            window.bytes().fill(t as u8);
                            let began = Instant::now();
                            pages_written(page, 0, PAGE);
                            let took = (looked + began.elapsed()).as_nanos() as u64;
                            waited.fetch_add(took, Ordering::Relaxed);
                            worst.fetch_max(took, Ordering::Relaxed);
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });
            let n = done.load(Ordering::Relaxed).max(1);
            println!(
                "mapping {mapping}: {:.0} requests/s, {:.1} us of each on lookups and notices (worst {:.1} ms); {:.0} maps/s",
                n as f64 / 3.0,
                waited.load(Ordering::Relaxed) as f64 / n as f64 / 1e3,
                worst.load(Ordering::Relaxed) as f64 / 1e6,
                maps.load(Ordering::Relaxed) as f64 / 3.0
            );
        }
        assert_eq!(XMemFreePhysicalPages(1024, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = page_table().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_is_busy_from_a_quarter_of_the_time_to_a_twentieth() {
        let ms = std::time::Duration::from_millis;
        assert!(!busy_after(false, ms(60), ms(250)) && busy_after(false, ms(70), ms(250)));
        assert!(busy_after(true, ms(15), ms(250)) && !busy_after(true, ms(12), ms(250)));
        // A window without a call, then one short one.
        assert!(!busy_after(true, ms(5), ms(4000)));
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}
