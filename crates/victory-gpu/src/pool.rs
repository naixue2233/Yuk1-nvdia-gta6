//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// (Fox V, `VICTORY_PAGE_POOL`) Texture copies on shared 64 KiB pages.
mod pages;
pub(crate) use pages::ready_on as pages_ready;
pub(crate) use pages::adopt as pages_adopt;

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them.
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
const STAGING_KEPT: usize = 8;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    let pressure = crate::residency::pressured();
    if let Some([budget, used, ..]) = memory {
        CARD[0].store(budget, Ordering::Relaxed);
        CARD[1].store(used, Ordering::Relaxed);
        if let Ok(pool) = TEXTURES.try_lock() {
            CARD[2].store(pool.blocks() as u64, Ordering::Relaxed);
        }
    }
    let limit = spares(memory, crate::host::keep_freed() && !pressure);
    SPARES.store(limit, Ordering::Relaxed);
    let tick = now();
    // A full pool can become idle while another subsystem needs the VRAM.
    // A busy allocator observes the new limit on its next allocation or tick.
    if let Ok(mut pool) = TEXTURES.try_lock() {
        pool.reclaim(tick, limit, true);
        pool.drain = drain();
        pool.mark_draining();
        // (Fox V) The meters copy under the lock and write after it.
        #[cfg(feature = "diagnostics")]
        {
            static LAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
            let drained = ((pool.drain.is_some() || cap().is_some()) && tick >= LAST[0].load(Ordering::Relaxed) + 5_000).then(|| {
                LAST[0].store(tick, Ordering::Relaxed);
                drain_line(&pool)
            });
            let sized = (sizes() && tick >= LAST[1].load(Ordering::Relaxed) + 30_000).then(|| {
                LAST[1].store(tick, Ordering::Relaxed);
                sizes_copy(&pool)
            });
            drop(pool);
            if let Some(line) = drained {
                tracing::info!("memory housekeeping, {line}");
            }
            if let Some(copy) = sized {
                tracing::info!("memory housekeeping, {}", sizes_line(&copy, tick));
            }
        }
    }
    // (`VICTORY_PAGE_POOL`) Rested pages free, empty heaps past the kept ones
    // go; its line every 5 s (no lock of the blocks held here).
    pages::tick(tick, limit);
    #[cfg(feature = "diagnostics")]
    if pages::mode() != pages::Mode::Off {
        static LAST: AtomicU64 = AtomicU64::new(0);
        if tick >= LAST.load(Ordering::Relaxed) + 5_000 {
            LAST.store(tick, Ordering::Relaxed);
            let [budget, raw] = [0, 1].map(|i| CARD[i].load(Ordering::Relaxed));
            let net = raw.saturating_sub(texture_room());
            tracing::info!("memory housekeeping, {}", pages::line(budget, raw, net));
        }
    }
    if let Ok(mut pool) = STAGING.try_lock() {
        let staging_limit = if crate::residency::critical_commit() {
            0
        } else if pressure {
            1
        } else {
            staging_spares().map_or(limit, |most| most.min(limit))
        };
        pool.reclaim(tick, staging_limit, true);
    }
}

/// (Fox port C) The most empty blocks of staging buffers kept while the PC
/// has room (`VICTORY_STAGING_SPARE_BLOCKS`; unset: as many as texture
/// blocks, [`SPARE_KEPT`] = 4 GiB of the PC's memory below 85 % of the card,
/// cool_fox's behaviour). Staging blocks are the PC's own memory, not the
/// card's, so the device-loss guard of [`crate::host::keep_freed`] is about
/// texture blocks; run-101 held 2,560 MB of staging blocks at 37 GB private
/// on a 32 GB PC. A/B candidate: 8 (`STAGING_KEPT`, 512 MiB).
fn staging_spares() -> Option<usize> {
    static MOST: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| std::env::var("VICTORY_STAGING_SPARE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()))
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

/// (Fox F, memory audit item 1) Milliseconds until a piece of a staging block
/// given back is used again (`VICTORY_STAGING_REST_MS`; unset: [`REST`], as
/// cool_fox). A staging piece comes back only when the last host list that
/// recorded its copy is reset, and a list is reset only after every
/// submission of it has run ([`crate::list`], `host::HostList::reset`): the
/// card is done reading it then, so the rest is a second guard only. At
/// 130-890 MB/s of uploads, 2.5 s of rest held 0.7-3.4 GB of staging blocks
/// for 60-240 MB live (run-121). A/B candidates: 250, 0. Risk: wrong texels if
/// some path drops a staging buffer before its copy ran (none known).
/// Staging blocks are upload heaps in the PC's memory: no texture lies in
/// them, so the device-loss guard of [`ahead`] is not about them.
fn staging_rest() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = std::env::var("VICTORY_STAGING_REST_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(REST);
        if ms != REST {
            tracing::info!(ms, "staging pieces rest {ms} ms before they are used again (VICTORY_STAGING_REST_MS; default {REST})");
        }
        ms
    })
}

/// (Fox F, memory audit item 1) `VICTORY_STAGING_PACK=1`: a staging piece goes
/// into the fullest block it fits (as `VICTORY_HEAP_PACK` for textures), and
/// at most [`STAGING_PACKED_KEPT`] empty staging blocks stay, so blocks
/// empty and go. Default off = cool_fox: first block with room, up to
/// [`STAGING_KEPT`] empty blocks.
fn staging_pack() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_STAGING_PACK").is_ok_and(|v| v.trim() == "1"))
}

/// Empty staging blocks kept with [`staging_pack`] (128 MiB).
const STAGING_PACKED_KEPT: usize = 2;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch;
/// and the same by (length, start), for best fit ([`classes`]).
struct Ranges(BTreeMap<u64, u64>, std::collections::BTreeSet<(u64, u64)>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]), std::collections::BTreeSet::from([(size, 0)]))
    }

    fn insert(&mut self, start: u64, len: u64) {
        self.0.insert(start, len);
        self.1.insert((len, start));
    }

    fn remove(&mut self, start: u64) -> Option<u64> {
        let len = self.0.remove(&start)?;
        self.1.remove(&(len, start));
        Some(len)
    }

    /// Cuts `len` bytes at `at` out of the free range `[start, start + size)`.
    fn cut(&mut self, start: u64, size: u64, at: u64, len: u64) -> u64 {
        self.remove(start);
        if at > start {
            self.insert(start, at - start);
        }
        if at + len < start + size {
            self.insert(at + len, start + size - at - len);
        }
        at
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        Some(self.cut(start, size, at, len))
    }

    /// (`VICTORY_POOL_CLASSES`) The smallest free range `len` bytes at a
    /// multiple of `align` fit in: its length and start, and the place.
    fn best(&self, len: u64, align: u64) -> Option<(u64, u64, u64)> {
        self.1.range((len, 0)..).find_map(|&(size, start)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((size, start, at))
        })
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.remove(before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.remove(start + len) {
            len += size;
        }
        self.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// (`VICTORY_HEAP_PACK`) Bytes of `free`: taken and given exactly.
    room: u64,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
    /// (`VICTORY_POOL_CLASSES`) The size class of its pieces ([`class_of`]);
    /// [`NO_CLASS`] while it is empty.
    class: u8,
    /// (`VICTORY_POOL_DRAIN`) New pieces go elsewhere while any other block
    /// can take them, so that it empties and goes ([`drain`]).
    draining: bool,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    retry_growth_at: u64,
    /// Empty blocks let go since the start.
    given_back: u64,
    /// (`VICTORY_HEAP_PACK`) Pieces go into the fullest block they fit.
    pack: bool,
    /// Milliseconds a piece given back rests ([`REST`]; staging:
    /// [`staging_rest`]).
    rest: u64,
    /// (`VICTORY_POOL_CLASSES`) Pieces go by size class and best fit, and a
    /// new block is made past the raw cap ([`Cap`]) only if no other block
    /// can take the piece.
    classes: Option<Cap>,
    /// (`VICTORY_POOL_DRAIN`) The fill percentage blocks are drained to.
    drain: Option<u64>,
    /// (`VICTORY_POOL_CAP`, without classes) New blocks, made ahead or not,
    /// only under the raw cap while any block can take the piece.
    capped: Option<Cap>,
    /// (`VICTORY_POOL_SIZES`) Pieces taken and not given back, by block and
    /// start: length and when (ms).
    live: Option<std::collections::HashMap<(usize, u64), (u64, u64)>>,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool {
            blocks: Vec::new(),
            resting: VecDeque::new(),
            spare: 0,
            most,
            retry_growth_at: 0,
            given_back: 0,
            pack: false,
            rest: REST,
            classes: None,
            drain: None,
            capped: None,
            live: None,
        }
    }

    /// (`VICTORY_HEAP_PACK`) Of the blocks `ripe` takes, the fullest one
    /// `len` fits in (lowest place in it), and that place.
    fn take_fullest(&mut self, len: u64, align: u64, ripe: Ripe, skip: bool) -> Option<(usize, u64)> {
        let mut order: Vec<(u64, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.as_ref().filter(|b| b.room >= len && ripe.is(b) && !(skip && b.draining)).map(|b| (b.room, i)))
            .collect();
        order.sort_unstable();
        order.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)))
    }

    /// The place for a piece without classes: fullest block (pack) or first
    /// block with room among ripe ones, then the oldest unripe one; `skip`:
    /// none of the blocks being drained ([`drain`]).
    fn find(&mut self, len: u64, align: u64, ripe: Ripe, skip: bool) -> Option<(usize, u64)> {
        let open = |b: &Block<B>| !(skip && b.draining);
        let mut found = None;
        if self.pack {
            found = self.take_fullest(len, align, ripe, skip);
        } else {
            for (i, block) in self.blocks.iter_mut().enumerate() {
                if let Some(block) = block
                    && ripe.is(block)
                    && open(block)
                    && let Some(start) = block.free.take(len, align)
                {
                    found = Some((i, start));
                    break;
                }
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> = self
                .blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.as_ref().filter(|b| open(b)).map(|b| (b.made, i)))
                .collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        found
    }

    /// (`VICTORY_POOL_CAP`, without classes) The place for a piece, by tier:
    /// a ripe block holding something and not being drained, one being
    /// drained, an empty ripe block, then the oldest unripe one; within a
    /// tier the fullest block that fits (`pack`) or the first; the place
    /// lowest in it. And whether the block is being drained. One pass over
    /// the blocks.
    fn find_capped(&mut self, len: u64, align: u64, ripe: Ripe) -> Option<(usize, u64, bool)> {
        // Per tier the best (key, block): key = room (pack) or index, the
        // unripe tier by when the block was made.
        let mut best: [Option<(u64, usize)>; 4] = [None; 4];
        for (i, b) in self.blocks.iter().enumerate() {
            let Some(b) = b else { continue };
            if b.room < len || b.free.best(len, align).is_none() {
                continue;
            }
            let (tier, key) = if !ripe.is(b) {
                (3, b.made)
            } else if b.free.whole(BLOCK) {
                (2, i as u64)
            } else {
                (usize::from(b.draining), if self.pack { b.room } else { i as u64 })
            };
            if best[tier].is_none_or(|(k, _)| key < k) {
                best[tier] = Some((key, i));
            }
        }
        // The unripe tier only with blocks made ahead, as before.
        let tiers = if ripe.ahead > 0 { 4 } else { 3 };
        let i = best[..tiers].iter().flatten().next()?.1;
        let block = self.blocks[i].as_mut()?;
        let draining = block.draining;
        Some((i, block.free.take(len, align)?, draining))
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        if let Some(cap) = self.classes {
            return self.take_classed(len, align, now, ripe, cap, &mut make);
        }
        let found = if self.capped.is_some() {
            // (`VICTORY_POOL_CAP`) A hole in any ripe block before an empty
            // or a new one.
            let found = self.find_capped(len, align, ripe);
            if let Some((_, _, draining)) = found
                && draining
            {
                DRAIN[DRAIN_FALLBACK].fetch_add(1, Ordering::Relaxed);
            }
            found.map(|(i, at, _)| (i, at))
        } else {
            // (`VICTORY_POOL_DRAIN`, without the cap: never with this build's
            // switches) Blocks being drained first not at all, then as before.
            let mut found = self.find(len, align, ripe, true);
            if found.is_none() && self.drain.is_some() {
                found = self.find(len, align, ripe, false);
                if found.is_some() {
                    DRAIN[DRAIN_FALLBACK].fetch_add(1, Ordering::Relaxed);
                }
            }
            found
        };
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let memory = self.grow(now, &mut make)?;
                if let Some(cap) = self.capped {
                    // No block can take it: a new one, under the cap or past
                    // it (no texture is left without memory), counted.
                    CAPPED[if cap.over(self.blocks()) { CAP_PAST } else { CAP_NEW }].fetch_add(1, Ordering::Relaxed);
                }
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            // (`VICTORY_POOL_CAP`) Blocks made ahead only under the cap.
            let held = empty < ripe.ahead && self.capped.is_some_and(|cap| cap.over(self.blocks() + 1));
            if held {
                CAPPED[CAP_AHEAD_HELD].fetch_add(1, Ordering::Relaxed);
            }
            if empty < ripe.ahead
                && !held
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        block.room = block.room.saturating_sub(len);
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    /// (`VICTORY_POOL_CLASSES`) The place [`Pool::take_classed`] takes and
    /// how, and whether the block is one being drained ([`drain`]): the
    /// smallest free range the piece fits (then the fullest block, then the
    /// lowest), by tier.
    #[allow(clippy::type_complexity)]
    fn select_classed(
        &self,
        len: u64,
        align: u64,
        ripe: Ripe,
        class: u8,
        over: bool,
    ) -> (Option<(usize, u64, u64, u64)>, usize, bool) {
        // (Fox V) One pass over the blocks for every tier (it was a pass per
        // tier, up to four): each block's best fit once, kept per tier as the
        // smallest (range, room, index).
        // Tiers 0-4 leave out blocks being drained, 5-9 the same with them:
        // 0 its class, ripe; 1 empty, ripe; 2 its class or empty, ripe or
        // not; 3 any block; 4 any ripe block.
        type Best = Option<((u64, u64, usize), (usize, u64, u64, u64))>;
        let mut best: [Best; 10] = [None; 10];
        for (i, b) in self.blocks.iter().enumerate() {
            let Some(b) = b else { continue };
            if b.room < len {
                continue;
            }
            let Some((size, start, at)) = b.free.best(len, align) else { continue };
            let entry = ((size, b.room, i), (i, start, size, at));
            let ripe_here = ripe.is(b);
            let tiers = [
                b.class == class && ripe_here,
                b.class == NO_CLASS && ripe_here,
                b.class == class || b.class == NO_CLASS,
                true,
                ripe_here,
            ];
            for (t, fits) in tiers.into_iter().enumerate() {
                if !fits {
                    continue;
                }
                for t in [t + 5, t].into_iter().filter(|&t| t >= 5 || !b.draining) {
                    if best[t].is_none_or(|(k, _)| entry.0 < k) {
                        best[t] = Some(entry);
                    }
                }
            }
        }
        // Without draining as before: its class, an empty block, an unripe
        // block, any block past the cap (tiers 0-4 then hold every block).
        // (`VICTORY_POOL_DRAIN`) The same leaving out blocks being drained,
        // then with them, before a new block. Past the cap a hole in any ripe
        // block, also one being drained, comes before an empty block: an
        // emptied block is not filled again at once (run-157: 11 blocks
        // emptied while drained, 9 of them taken again as empty blocks).
        let order: &[(usize, usize)] = match (self.drain.is_some(), over) {
            (false, _) => &[(0, CLASS_HOLE), (1, CLASS_EMPTY), (2, CLASS_UNRIPE), (3, CLASS_OTHER)],
            (true, false) => &[(0, CLASS_HOLE), (5, CLASS_HOLE), (1, CLASS_EMPTY), (2, CLASS_UNRIPE), (7, CLASS_UNRIPE)],
            (true, true) => &[
                (0, CLASS_HOLE),
                (4, CLASS_OTHER),
                (5, CLASS_HOLE),
                (9, CLASS_OTHER),
                (1, CLASS_EMPTY),
                (2, CLASS_UNRIPE),
                (3, CLASS_OTHER),
                (8, CLASS_OTHER),
            ],
        };
        for &(t, how) in order {
            if (t % 5 == 2 && ripe.ahead == 0) || (t % 5 == 3 && !over) {
                continue;
            }
            if let Some((_, place)) = best[t] {
                let draining = self.blocks[place.0].as_ref().is_some_and(|b| b.draining);
                return (Some(place), how, draining);
            }
        }
        (None, CLASS_HOLE, false)
    }

    /// (`VICTORY_POOL_CLASSES`) [`Pool::take_ripe`] by size class and best
    /// fit. A piece goes into the best fit among ripe blocks of its class,
    /// then empty ripe ones; without one (and no unripe block of its class or
    /// empty), a new block while the card's raw use with it stays under the
    /// cap; past the cap the best fit in any block; with none at all a new
    /// block after all (as without classes: no texture is left without
    /// memory), counted.
    fn take_classed(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        cap: Cap,
        make: &mut impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        let class = class_of(len);
        let over = cap.over(self.blocks());
        let (found, how, draining) = self.select_classed(len, align, ripe, class, over);
        if draining {
            DRAIN[DRAIN_FALLBACK].fetch_add(1, Ordering::Relaxed);
        }
        let (i, at) = match found {
            Some((i, start, size, at)) => {
                let b = self.blocks[i].as_mut()?;
                b.free.cut(start, size, at, len);
                CLASSED[how].fetch_add(1, Ordering::Relaxed);
                (i, at)
            }
            None => {
                let memory = self.grow(now, make)?;
                CLASSED[if over { CLASS_PAST_CAP } else { CLASS_NEW }].fetch_add(1, Ordering::Relaxed);
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK) && b.class == NO_CLASS).count();
            // (The block just cut from is not empty any more.)
            if empty < ripe.ahead {
                // Blocks made ahead are made under the cap only (none is
                // needed for a texture yet).
                if cap.over(self.blocks() + 1) {
                    CLASSED[CLASS_AHEAD_CAPPED].fetch_add(1, Ordering::Relaxed);
                } else if let Some(memory) = self.grow(now, make) {
                    self.add(memory, ripe.started);
                }
            }
        }
        let block = self.blocks[i].as_mut()?;
        // (`VICTORY_POOL_DRAIN`) A piece put in another class's block past the
        // cap does not make it a block of its class (that made 27 blocks of
        // 8 MiB pieces "small" in one 5 s step of run-130).
        if self.drain.is_none() || block.class == NO_CLASS {
            block.class = class;
        }
        block.room = block.room.saturating_sub(len);
        let again = at < block.used_to;
        block.used_to = block.used_to.max(at + len);
        Some((Piece { block: i, start: at, len }, block.memory.clone(), again))
    }

    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] =
            Some(Block {
            memory,
            free: Ranges::new(BLOCK),
            room: BLOCK,
            used_to: 0,
            made,
            since: crate::host::now_ms(),
            class: NO_CLASS,
            draining: false,
        });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        if let Some(live) = self.live.as_mut() {
            live.remove(&(piece.block, piece.start));
        }
        self.resting.push_back((now + self.rest, piece));
    }

    /// (`VICTORY_POOL_DRAIN`) Marks the sparsest blocks that hold something
    /// as being drained: as many as the blocks holding something exceed what
    /// their bytes in use (resting pieces included) need at the fill
    /// percentage; the others are not. Once a second ([`room`]).
    fn mark_draining(&mut self) {
        let Some(pct) = self.drain else { return };
        let began = std::time::Instant::now();
        // Sparsest first; one being drained counts a quarter emptier, so the
        // set stays while others thin out a little.
        let mut used: Vec<(u64, u64, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let b = b.as_ref().filter(|b| !b.free.whole(BLOCK))?;
                let u = BLOCK - b.room;
                Some((if b.draining { u - u / 4 } else { u }, u, i))
            })
            .collect();
        let live: u64 = used.iter().map(|u| u.1).sum();
        let needed = (u128::from(live) * 100).div_ceil(u128::from(BLOCK) * u128::from(pct.max(1))) as usize;
        let excess = used.len().saturating_sub(needed);
        used.sort_unstable();
        for b in self.blocks.iter_mut().flatten() {
            b.draining = false;
        }
        let mut held = 0;
        for &(_, u, i) in used.iter().take(excess) {
            if let Some(b) = self.blocks[i].as_mut() {
                b.draining = true;
                held += u;
            }
        }
        DRAIN_NOW[0].store(excess as u64, Ordering::Relaxed);
        DRAIN_NOW[1].store(held, Ordering::Relaxed);
        DRAIN_NOW[2].store(needed as u64, Ordering::Relaxed);
        PLACING[4].store(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    fn rested(&mut self, now: u64) {
        self.reclaim(now, SPARES.load(Ordering::Relaxed), false);
    }

    fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                block.room = block.room.saturating_add(piece.len).min(BLOCK);
                if block.free.whole(BLOCK) {
                    block.class = NO_CLASS;
                    if std::mem::take(&mut block.draining) {
                        DRAIN[DRAIN_EMPTIED].fetch_add(1, Ordering::Relaxed);
                    }
                    any = true;
                }
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let (mut spare, mut gone) = (0, 0);
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                        gone += 1;
                    }
                }
            }
            self.given_back += gone;
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new(STAGING_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first.
pub(crate) fn texture_room() -> u64 {
    let blocks = lock(&TEXTURES).blocks() as u64;
    // (`VICTORY_PAGE_POOL`) Free and resting pages and empty page heaps too
    // (taken after the blocks' lock went: the page pool asks the blocks'
    // lock only with `try_lock`).
    (blocks * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed)) + pages::room()
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    [lock(&TEXTURES).blocks() as u64 * (BLOCK >> 20), lock(&STAGING).blocks() as u64 * (BLOCK >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let mut pool = lock(&STAGING);
    pool.rest = staging_rest();
    if staging_pack() {
        pool.pack = true;
        pool.most = STAGING_PACKED_KEPT;
    }
    let taken = pool.take(bytes, 512, now(), || {
        // Only gate speculative bindless prefetch on staging-pool growth
        // headroom; table-bound textures and decoration bakes are sampled
        // immediately and would otherwise read uninitialized/reused VRAM.
        let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
        if speculative && !crate::residency::allow_staging_growth(false) {
            return None;
        }
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        Some(Upload(buffer, data))
    });
    drop(pool);
    let (piece, memory, again) = taken?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
    /// The texture's interface pointer ([`BLOCK_OF`]).
    texture: usize,
}

/// (Fox port C, from victory-dev; `VICTORY_HEAP_PACK`; default off = cool_fox:
/// first block with room, lowest place) Texture blocks are packed so that sparse
/// ones empty and go back (fragmentation: run-103 held 16.3 GB of the card "counting
/// free heap space" for 13.0 GB in use, on a 14.9 GB budget):
/// - `=1` or `=place`: a new piece goes into the fullest block it fits (lowest
///   place in it), so holes in full blocks fill before sparse blocks get more;
/// - `=1` also: the room rule (`resource::let_go_idle`, `vram_keep`) takes the
///   copies it may take (its own idle times, unchanged) sparsest block first,
///   least recently used within a 1/32 occupancy step (copies of no block with
///   the sparsest): their blocks empty and go (2 kept above 85 % of the card).
/// Nothing live is moved.
pub(crate) fn heap_pack() -> (bool, bool) {
    static ON: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_HEAP_PACK").as_deref().map(str::trim) {
        Ok("1") => (true, true),
        Ok("place") => (true, false),
        _ => (false, false),
    })
}

/// (Fox F, holes design step 2; `VICTORY_POOL_CLASSES=1`, default off =
/// cool_fox: one first-fit pool) Texture blocks by size class, best fit, and a
/// cap on making new blocks by the card's raw use, so that holes are taken
/// again instead of new blocks made (run-121: 2.0-2.9 GB free inside blocks
/// that hold something; raw over budget by that much):
/// - pieces of up to `VICTORY_POOL_CLASS_KB` (default `1024,8192`) KiB, of up
///   to the second figure, and larger ones go in blocks of their class only;
///   an empty block takes any class;
/// - in those, the smallest free range the piece fits (then the fullest
///   block) instead of the first block's lowest one;
/// - a new block only while raw use (as of the last second, plus the blocks
///   made since) with it stays under `VICTORY_POOL_RAW_PCT` (default 95) % of
///   the budget; past that the best fit in a block of any class, and with
///   none a new block after all (counted: nothing is left without memory).
/// Nothing live is moved; what is let go is unchanged (the eviction form of a
/// raw cap, `VICTORY_VRAM_RAW_PCT`, was rejected in run-107). `VICTORY_HEAP_PACK`
/// still orders what the room rule lets go; its placement is this one's.
pub(crate) fn classes() -> Option<Cap> {
    static ON: std::sync::OnceLock<Option<Cap>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var("VICTORY_POOL_CLASSES").is_ok_and(|v| v.trim() == "1") {
            return None;
        }
        let kb: Vec<u64> = std::env::var("VICTORY_POOL_CLASS_KB")
            .ok()
            .map(|v| v.split(',').filter_map(|k| k.trim().parse().ok()).collect::<Vec<u64>>())
            .filter(|k| k.len() == 2 && k[0] < k[1])
            .unwrap_or_else(|| vec![1024, 8192]);
        CLASS_LIMITS[0].store(kb[0] << 10, Ordering::Relaxed);
        CLASS_LIMITS[1].store(kb[1] << 10, Ordering::Relaxed);
        let pct = std::env::var("VICTORY_POOL_RAW_PCT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(95u64).min(100);
        tracing::info!(
            small_kb = kb[0],
            medium_kb = kb[1],
            raw_pct = pct,
            "texture blocks by size class, best fit, new blocks under {pct} % of the card's budget (VICTORY_POOL_CLASSES=1)"
        );
        Some(Cap { pct })
    })
}

/// Pieces up to these bytes are of class 0, then 1; larger ones 2.
static CLASS_LIMITS: [AtomicU64; 2] = [AtomicU64::new(1 << 20), AtomicU64::new(8 << 20)];
/// The class of an empty block.
const NO_CLASS: u8 = u8::MAX;
/// Size classes.
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
const CLASSES: usize = 3;

fn class_of(len: u64) -> u8 {
    if len <= CLASS_LIMITS[0].load(Ordering::Relaxed) {
        0
    } else if len <= CLASS_LIMITS[1].load(Ordering::Relaxed) {
        1
    } else {
        2
    }
}

/// The raw cap on new blocks ([`classes`]).
#[derive(Clone, Copy)]
pub(crate) struct Cap {
    pct: u64,
}

/// The card's budget and raw use at the last [`room`], and the texture
/// blocks alive then.
static CARD: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

impl Cap {
    /// Whether making a block, with `blocks` alive, puts the card's raw use
    /// over the cap (no figure from the system yet: never).
    fn over(self, blocks: usize) -> bool {
        let [budget, used, then] = [0, 1, 2].map(|i| CARD[i].load(Ordering::Relaxed));
        if budget == 0 {
            return false;
        }
        let since = (blocks as u64).saturating_sub(then) * BLOCK;
        u128::from(used + since + BLOCK) * 100 > u128::from(budget) * u128::from(self.pct)
    }
}

/// (`VICTORY_POOL_CLASSES`) Since the start, placements: in a hole of the
/// piece's class (best fit), in an empty block, in an unripe block, in a
/// block of another class past the cap; new blocks made under the cap, made
/// past it for want of any room (the fallback); blocks made ahead that the
/// cap held back.
static CLASSED: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const CLASS_HOLE: usize = 0;
const CLASS_EMPTY: usize = 1;
const CLASS_UNRIPE: usize = 2;
const CLASS_OTHER: usize = 3;
const CLASS_NEW: usize = 4;
const CLASS_PAST_CAP: usize = 5;
const CLASS_AHEAD_CAPPED: usize = 6;

/// For the housekeeping line ([`classes`]): blocks and free bytes by class,
/// raw / net / budget and the placements.
#[cfg(feature = "diagnostics")]
pub(crate) fn classes_line(budget: u64, raw: u64, net: u64) -> String {
    let mb = |b: u64| b >> 20;
    let (mut blocks, mut free) = ([0u64; CLASSES + 1], [0u64; CLASSES + 1]);
    for b in lock(&TEXTURES).blocks.iter().flatten() {
        let c = if b.class == NO_CLASS { CLASSES } else { usize::from(b.class).min(CLASSES - 1) };
        blocks[c] += 1;
        free[c] += b.room;
    }
    let n: [u64; 7] = std::array::from_fn(|i| CLASSED[i].load(Ordering::Relaxed));
    format!(
        "texture blocks by class (VICTORY_POOL_CLASSES={}; <= {} KiB / <= {} KiB / larger / empty): {} / {} / {} / {} blocks, \
         free inside {} / {} / {} MB (holes {} MB); card raw {} MB, net {} MB, budget {} MB, cap {} % (VICTORY_POOL_RAW_PCT); \
         placements since the start: {} best-fit holes, {} empty blocks, {} unripe blocks, {} in another class past the cap, \
         {} new blocks under the cap, {} new blocks past the cap for want of room (fallback), {} blocks made ahead held back by the cap",
        u8::from(classes().is_some()),
        CLASS_LIMITS[0].load(Ordering::Relaxed) >> 10,
        CLASS_LIMITS[1].load(Ordering::Relaxed) >> 10,
        blocks[0],
        blocks[1],
        blocks[2],
        blocks[3],
        mb(free[0]),
        mb(free[1]),
        mb(free[2]),
        mb(free[0] + free[1] + free[2]),
        mb(raw),
        mb(net),
        mb(budget),
        classes().map_or(0, |c| c.pct),
        n[0],
        n[1],
        n[2],
        n[3],
        n[4],
        n[5],
        n[6],
    )
}

/// (Fox V, holes; `VICTORY_POOL_DRAIN=1`, default off = as before) Texture
/// blocks follow what is in them. run-130 (`VICTORY_POOL_CLASSES=1`): the pool
/// grew from 18 to 187 blocks in the 16 s the world loaded and stayed at
/// 177-187 (11.3 GB) for ~8.0 GB of copies alive (the room rule's level); no
/// piece in the run lacked a hole (fallback 0), no block was under 25 % in
/// use: the holes are room left from the load, not gaps too small to use, and
/// no block empties because every one keeps being refilled. So, once a second,
/// the blocks that hold something past what their bytes need at
/// `VICTORY_POOL_DRAIN_PCT` (default 90) % are marked, the sparsest ones; a new
/// piece goes into them only when no other block can take it (counted), and
/// always before a new block is made. Their copies go as copies go today (the
/// title's turnover; `VICTORY_HEAP_PACK=1` lets the sparsest go first), and an
/// emptied block goes back as empty blocks do (2 kept under pressure). Nothing
/// live is moved, no copy is let go for it, no block is made for it.
pub(crate) fn drain() -> Option<u64> {
    static ON: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var("VICTORY_POOL_DRAIN").is_ok_and(|v| v.trim() == "1") {
            return None;
        }
        let pct = std::env::var("VICTORY_POOL_DRAIN_PCT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(90u64)
            .clamp(50, 100);
        tracing::info!(pct, "texture blocks drained to {pct} % in use (VICTORY_POOL_DRAIN=1)");
        Some(pct)
    })
}

/// (Fox V; `VICTORY_POOL_CAP=1`, default off; on with `VICTORY_POOL_DRAIN=1`)
/// The raw cap of `VICTORY_POOL_CLASSES` without classes (run-159: drain
/// without it grew the pool 171 -> 194 blocks, raw 15.7 -> 18.1 GB, through
/// the blocks made ahead, which nothing capped): a piece takes a hole in any
/// ripe block (one being drained too) before an empty block, an unripe one or
/// a new one; blocks are made ahead only while the card's raw use with one
/// more stays under `VICTORY_POOL_RAW_PCT` (95) % of the budget; and a new
/// block for a piece past the cap only when no block can take it (counted).
/// With classes, their own cap is used.
pub(crate) fn cap() -> Option<Cap> {
    static ON: std::sync::OnceLock<Option<Cap>> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var("VICTORY_POOL_CAP").is_ok_and(|v| v.trim() == "1") && drain().is_none() {
            return None;
        }
        let pct = std::env::var("VICTORY_POOL_RAW_PCT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(95u64).min(100);
        tracing::info!(pct, "new texture blocks only under {pct} % of the card's budget while a block has room (VICTORY_POOL_CAP)");
        Some(Cap { pct })
    })
}

/// (`VICTORY_POOL_CAP`) Since the start: new blocks for a piece no block could
/// take, under the cap and past it; blocks made ahead the cap held back.
static CAPPED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
const CAP_NEW: usize = 0;
const CAP_PAST: usize = 1;
const CAP_AHEAD_HELD: usize = 2;

/// (`VICTORY_POOL_DRAIN`) Since the start: pieces put in a block being drained
/// for want of another; blocks emptied while being drained.
static DRAIN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
const DRAIN_FALLBACK: usize = 0;
const DRAIN_EMPTIED: usize = 1;
/// At the last marking: blocks being drained, their bytes in use, blocks
/// the bytes in use need.
static DRAIN_NOW: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// (`VICTORY_POOL_DRAIN` / `VICTORY_POOL_SIZES`) Since the last drain line:
/// texture placements, their microseconds under the pool's lock (a block
/// made among them), the longest, and microseconds waited for the lock;
/// the microseconds of the last marking.
static PLACING: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// (Fox V; `VICTORY_POOL_SIZES=1`, a meter, default off) Every 30 s a line of
/// the texture pieces alive and the holes by size, the sizes asked for since
/// the last line, and the blocks by the age of their oldest piece.
pub(crate) fn sizes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_POOL_SIZES").is_ok_and(|v| v.trim() == "1"))
}

/// Power-of-two size steps from 4 KiB (step 0: < 8 KiB) to 64 MiB.
const STEPS: usize = 15;

fn step(len: u64) -> usize {
    (len.max(1).ilog2() as usize).saturating_sub(12).min(STEPS - 1)
}

/// (`VICTORY_POOL_SIZES`) Texture pieces asked for since the last sizes line,
/// by [`step`]: count and bytes.
static ASKED: [[AtomicU64; STEPS]; 2] = [const { [const { AtomicU64::new(0) }; STEPS] }; 2];

/// What the sizes line needs, copied under the pool's lock (a copy of some
/// 26,000 pieces and 3,500 holes): the pieces alive, (block, start) and
/// (length, made), and the hole lengths of each block holding something.
#[cfg(feature = "diagnostics")]
type Sizes = (Vec<((usize, u64), (u64, u64))>, Vec<(usize, Vec<u64>)>);

#[cfg(feature = "diagnostics")]
fn sizes_copy(pool: &Pool<ID3D12Heap>) -> Sizes {
    let live = pool.live.as_ref().map(|l| l.iter().map(|(k, v)| (*k, *v)).collect()).unwrap_or_default();
    let holes = pool
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.as_ref().filter(|b| !b.free.whole(BLOCK)).map(|b| (i, b.free.0.values().copied().collect())))
        .collect();
    (live, holes)
}

/// The sizes line ([`sizes`]) from [`sizes_copy`], outside the lock; `now`
/// in ms.
#[cfg(feature = "diagnostics")]
fn sizes_line((live, blocks): &Sizes, now: u64) -> String {
    use std::fmt::Write;
    let mb = |b: u64| b as f64 / f64::from(1 << 20);
    let label = |s: usize| if s + 1 == STEPS { format!(">={}M", 1u64 << (s + 12 - 20)) } else if s < 7 { format!("<{}K", 8u64 << s) } else { format!("<{}M", 1u64 << (s - 7)) };
    let mut out = String::new();
    // Pieces alive.
    let (mut n, mut bytes, mut small) = ([0u64; STEPS], [0u64; STEPS], [0u64; 2]);
    let mut exact: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut oldest: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    let ages = [10_000u64, 30_000, 60_000, 120_000, 300_000];
    let mut old_bytes = [0u64; 5];
    {
        for &((block, start), (len, made)) in live {
            let s = step(len);
            n[s] += 1;
            bytes[s] += len;
            *exact.entry(len).or_default() += 1;
            if !start.is_multiple_of(65536) {
                small[0] += 1;
                small[1] += len;
            }
            let o = oldest.entry(block).or_insert(made);
            *o = (*o).min(made);
            let age = now.saturating_sub(made);
            for (k, &a) in ages.iter().enumerate() {
                if age >= a {
                    old_bytes[k] += len;
                }
            }
        }
    }
    let total: u64 = bytes.iter().sum();
    let _ = write!(out, "texture pieces (VICTORY_POOL_SIZES=1): alive {} ({:.0} MB; {} at 4 KiB placement, {:.0} MB) by size:", n.iter().sum::<u64>(), mb(total), small[0], mb(small[1]));
    for s in 0..STEPS {
        if n[s] > 0 {
            let _ = write!(out, " {} {}/{:.0}MB", label(s), n[s], mb(bytes[s]));
        }
    }
    let mut top: Vec<(u64, u64)> = exact.into_iter().map(|(len, c)| (len * c, len)).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    let top_bytes: u64 = top.iter().take(30).map(|t| t.0).sum();
    let _ = write!(out, "; top 30 sizes ({:.0} % of bytes): ", if total > 0 { top_bytes as f64 * 100.0 / total as f64 } else { 0.0 });
    for (b, len) in top.iter().take(30) {
        let _ = write!(out, "{}K x{} ", len >> 10, b / len);
    }
    // Holes, in blocks holding something.
    let (mut hn, mut hb) = ([0u64; STEPS], [0u64; STEPS]);
    let mut blocks_by_age = [0u64; 6];
    for (i, holes) in blocks {
        for &len in holes {
            hn[step(len)] += 1;
            hb[step(len)] += len;
        }
        if let Some(&o) = oldest.get(i) {
            let age = now.saturating_sub(o);
            blocks_by_age[ages.iter().take_while(|&&a| age >= a).count()] += 1;
        }
    }
    let _ = write!(out, "; holes {} ({:.0} MB) by size:", hn.iter().sum::<u64>(), mb(hb.iter().sum()));
    for s in 0..STEPS {
        if hn[s] > 0 {
            let _ = write!(out, " {} {}/{:.0}MB", label(s), hn[s], mb(hb[s]));
        }
    }
    let asked: [[u64; STEPS]; 2] = std::array::from_fn(|k| std::array::from_fn(|s| ASKED[k][s].swap(0, Ordering::Relaxed)));
    let _ = write!(out, "; asked for in the last 30 s:");
    for s in 0..STEPS {
        if asked[0][s] > 0 {
            let _ = write!(out, " {} {}/{:.0}MB", label(s), asked[0][s], mb(asked[1][s]));
        }
    }
    // A hole is of use to the pieces asked for if one of them was no larger.
    let smallest = (0..STEPS).find(|&s| asked[0][s] > 0);
    let unusable: u64 = smallest.map_or(0, |m| (0..m).map(|s| hb[s]).sum());
    let _ = write!(
        out,
        "; holes smaller than any size asked for {:.0} MB; blocks by their oldest piece's age <10 s / 10-30 / 30-60 / 60-120 / 120-300 / >=300 s: {} / {} / {} / {} / {} / {}; \
         bytes of pieces older than 10 / 30 / 60 / 120 / 300 s: {:.0} / {:.0} / {:.0} / {:.0} / {:.0} MB",
        mb(unusable),
        blocks_by_age[0],
        blocks_by_age[1],
        blocks_by_age[2],
        blocks_by_age[3],
        blocks_by_age[4],
        blocks_by_age[5],
        mb(old_bytes[0]),
        mb(old_bytes[1]),
        mb(old_bytes[2]),
        mb(old_bytes[3]),
        mb(old_bytes[4]),
    );
    out
}

/// The drain line ([`drain`]), every 5 s.
#[cfg(feature = "diagnostics")]
fn drain_line(pool: &Pool<ID3D12Heap>) -> String {
    let mb = |b: u64| b >> 20;
    let [budget, raw] = [0, 1].map(|i| CARD[i].load(Ordering::Relaxed));
    let (mut blocks, mut empty, mut holes, mut draining_free) = (0u64, 0u64, 0u64, 0u64);
    for b in pool.blocks.iter().flatten() {
        blocks += 1;
        if b.free.whole(BLOCK) {
            empty += 1;
        } else {
            holes += b.room;
            if b.draining {
                draining_free += b.room;
            }
        }
    }
    let resting: u64 = pool.resting.iter().map(|r| r.1.len).sum();
    let held = HELD.load(Ordering::Relaxed);
    format!(
        "texture blocks drained (VICTORY_POOL_DRAIN={}, to {} %): {} blocks ({} MB), {} empty; held by textures {} MB, resting {} MB, \
         holes {} MB (of them in blocks being drained {} MB); blocks being drained {} holding {} MB, blocks the bytes in use need {}; \
         since the start: {} pieces put in a block being drained for want of another, {} blocks emptied while drained, {} empty blocks given back; \
         card raw {} MB, raw less texture room {} MB, budget {} MB; in the last 5 s {} placements took {} us under the lock \
         (longest {} us, new blocks included), {} us waiting for it; marking took {} us; \
         VICTORY_POOL_CAP={} ({} %): new blocks for a piece no block could take {} under the cap, {} past it; blocks made ahead held back {}",
        u8::from(pool.drain.is_some()),
        pool.drain.unwrap_or(0),
        blocks,
        blocks * (BLOCK >> 20),
        empty,
        mb(held),
        mb(resting),
        mb(holes),
        mb(draining_free),
        DRAIN_NOW[0].load(Ordering::Relaxed),
        mb(DRAIN_NOW[1].load(Ordering::Relaxed)),
        DRAIN_NOW[2].load(Ordering::Relaxed),
        DRAIN[DRAIN_FALLBACK].load(Ordering::Relaxed),
        DRAIN[DRAIN_EMPTIED].load(Ordering::Relaxed),
        pool.given_back,
        mb(raw),
        mb(raw.saturating_sub((blocks * BLOCK).saturating_sub(held))),
        mb(budget),
        PLACING[0].swap(0, Ordering::Relaxed),
        PLACING[1].swap(0, Ordering::Relaxed),
        PLACING[2].swap(0, Ordering::Relaxed),
        PLACING[3].swap(0, Ordering::Relaxed),
        PLACING[4].load(Ordering::Relaxed),
        u8::from(cap().is_some()),
        cap().map_or(0, |c| c.pct),
        CAPPED[CAP_NEW].load(Ordering::Relaxed),
        CAPPED[CAP_PAST].load(Ordering::Relaxed),
        CAPPED[CAP_AHEAD_HELD].load(Ordering::Relaxed),
    )
}

/// (`VICTORY_HEAP_PACK=1`) The block of each texture cut from one, by its
/// interface pointer.
static BLOCK_OF: Mutex<Option<std::collections::HashMap<usize, usize>>> = Mutex::new(None);

/// (`VICTORY_HEAP_PACK=1`) For the room rule: the block of each texture and the
/// bytes in use of each block (`None`: no block there), taken together.
pub(crate) struct Occupancy {
    of: std::collections::HashMap<usize, usize>,
    used: Vec<Option<u64>>,
}

impl Occupancy {
    pub(crate) fn now() -> Occupancy {
        let of = lock(&BLOCK_OF).clone().unwrap_or_default();
        let used = lock(&TEXTURES).blocks.iter().map(|b| b.as_ref().map(|b| BLOCK - b.room)).collect();
        Occupancy { of, used }
    }

    /// Bytes in use of the block `texture` (an interface pointer) lies in.
    pub(crate) fn used_of(&self, texture: usize) -> Option<u64> {
        self.of.get(&texture).and_then(|&i| self.used.get(i).copied().flatten())
    }

    /// The rank of `texture` for letting go: 0 for one in no block, else the
    /// occupancy step (1..=32) of its block.
    pub(crate) fn rank(&self, texture: usize) -> u64 {
        // (`VICTORY_PAGE_POOL`) A copy on pages empties no block: last.
        if !self.of.contains_key(&texture) && pages::holds(texture) {
            return PACK_STEPS;
        }
        pack_rank(self.used_of(texture), BLOCK)
    }
}

/// The steps of block occupancy candidates are ordered by ([`pack_rank`]).
const PACK_STEPS: u64 = 32;

/// A candidate's rank for letting go: the occupancy step of its block, `used`
/// bytes of `size`; 0 for a copy in no block.
pub(crate) fn pack_rank(used: Option<u64>, size: u64) -> u64 {
    match used {
        Some(used) if size > 0 => (used.min(size) * PACK_STEPS / size).min(PACK_STEPS - 1) + 1,
        _ => 0,
    }
}

/// Since the start: copies the room rule let go from blocks under half in use
/// and their bytes (`VICTORY_HEAP_PACK=1`).
pub(crate) static PACKED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// For the housekeeping line: texture blocks alive; of those holding
/// something, by occupancy (<25 %, 25-50 %, 50-75 %, >75 %) and their free
/// bytes; empty blocks; empty blocks let go since the start.
pub(crate) fn texture_blocks() -> [u64; 8] {
    let pool = lock(&TEXTURES);
    let mut out = [0u64; 8];
    for b in pool.blocks.iter().flatten() {
        out[0] += 1;
        if b.free.whole(BLOCK) {
            out[6] += 1;
            continue;
        }
        let used = BLOCK - b.room;
        out[1 + ((used * 4 / BLOCK) as usize).min(3)] += 1;
        out[5] += b.room;
    }
    out[7] = pool.given_back;
    out
}

/// (Fox F) For the memory line: staging blocks alive, of them empty; bytes of
/// pieces taken and not yet given back (live), bytes of pieces given back and
/// resting; empty staging blocks let go since the start; the rest (ms) and
/// whether pieces are packed.
#[cfg(feature = "diagnostics")]
pub(crate) fn staging_blocks() -> [u64; 7] {
    let pool = lock(&STAGING);
    let mut out = [0u64; 7];
    let mut used = 0;
    for b in pool.blocks.iter().flatten() {
        out[0] += 1;
        if b.free.whole(BLOCK) {
            out[1] += 1;
        }
        used += BLOCK - b.room;
    }
    out[3] = pool.resting.iter().map(|(_, p)| p.len).sum();
    out[2] = used.saturating_sub(out[3]);
    out[4] = pool.given_back;
    out[5] = pool.rest;
    out[6] = u64::from(pool.pack);
    out
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
            if heap_pack().1
                && let Some(of) = lock(&BLOCK_OF).as_mut()
            {
                of.remove(&gone.texture);
            }
            lock(&TEXTURES).give(gone.piece, now());
        }
        left
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    while recent.front().is_some_and(|r| r.1 + 1000 < now) {
        recent.pop_front();
    }
    recent.push_back((texture.as_raw() as usize, now));
}

/// Makes `texture` give `piece` back when the last reference to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece) {
    const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);
    let raw = texture.as_raw() as usize;
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece, texture: raw }));
    if heap_pack().1 {
        lock(&BLOCK_OF).get_or_insert_with(Default::default).insert(raw, piece.block);
    }
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most): 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    // (`VICTORY_PAGE_POOL`) A copy of 64 KiB or more on pages; else (or
    // verifying) a piece as before. Before the blocks' lock is taken.
    if pages::mode() != pages::Mode::Off {
        // SAFETY: the caller's description.
        if let Ok(made) = unsafe { pages::texture(device, &*desc, state, info.SizeInBytes) } {
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            crate::book::made(&made, || "a texture on pages".into());
            return Some(made);
        }
    }
    let asked = (drain().is_some() || cap().is_some() || sizes()).then(std::time::Instant::now);
    let mut pool = lock(&TEXTURES);
    if let Some(t) = asked {
        PLACING[3].fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
    pool.pack = heap_pack().0;
    pool.classes = classes();
    pool.drain = drain();
    pool.capped = cap();
    if sizes() && pool.live.is_none() {
        pool.live = Some(Default::default());
    }
    if sizes() {
        let s = step(info.SizeInBytes);
        ASKED[0][s].fetch_add(1, Ordering::Relaxed);
        ASKED[1][s].fetch_add(info.SizeInBytes, Ordering::Relaxed);
    }
    let timed = (pool.drain.is_some() || pool.capped.is_some() || sizes()).then(std::time::Instant::now);
    let taken = pool.take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: BLOCK,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            crate::residency::allocation_error(&e);
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap
            && let Err(e) = crate::host::resident(device, h).and_then(|()| crate::host::mapped(device))
        {
            tracing::warn!(error = %e, "a block of texture memory is not resident");
            return None;
        }
        heap
    });
    if let (Some(live), Some((piece, ..))) = (pool.live.as_mut(), &taken) {
        live.insert((piece.block, piece.start), (piece.len, now()));
    }
    drop(pool);
    if let Some(t) = timed {
        let us = t.elapsed().as_micros() as u64;
        PLACING[0].fetch_add(1, Ordering::Relaxed);
        PLACING[1].fetch_add(us, Ordering::Relaxed);
        PLACING[2].fetch_max(us, Ordering::Relaxed);
    }
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = taken?;
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                crate::book::block(&heap, &made, piece.start, BLOCK);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that set the card's figures ([`CARD`]) one at a time.
    pub(super) static CARD_TEST: Mutex<()> = Mutex::new(());

    #[test]
    fn packed_pieces_go_to_the_fullest_block_and_rooms_stay_exact() {
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        let mut n = 0u32;
        let mut make = || {
            n += 1;
            Some(n)
        };
        // Two blocks: the first nearly full, the second a quarter full.
        // (Pieces of more than half a block are not pooled without ripe blocks.)
        let (a, _, _) = pool.take(BLOCK / 2, 4096, 0, &mut make).unwrap();
        let (a2, _, _) = pool.take(BLOCK / 2 - (4 << 20), 4096, 0, &mut make).unwrap();
        assert_eq!(a.block, a2.block);
        let (b, _, _) = pool.take(BLOCK / 2, 4096, 0, &mut make).unwrap();
        assert_ne!(a.block, b.block);
        pool.give(b, 0);
        pool.reclaim(REST, SPARE_KEPT, false);
        let (_, _, _) = pool.take(BLOCK / 4, 4096, REST, &mut make).unwrap();
        pool.pack = true;
        let (c, _, _) = pool.take(1 << 20, 4096, REST, &mut make).unwrap();
        assert_eq!(c.block, a.block, "the fuller block takes the piece");
        pool.pack = false;
        let rooms: Vec<u64> = pool.blocks.iter().flatten().map(|b| b.room).collect();
        assert_eq!(rooms, vec![3 << 20, BLOCK - BLOCK / 4]);
        assert_eq!(pack_rank(None, BLOCK), 0);
        assert_eq!(pack_rank(Some(0), BLOCK), 1);
        assert_eq!(pack_rank(Some(BLOCK), BLOCK), PACK_STEPS);
    }

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || { calls.set(calls.get() + 1); None };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
    }

    #[test]
    fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
        let mut pool = Pool::new(STAGING_KEPT);
        let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
        let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
        pool.give(resting, 0);
        for i in 0..12 {
            pool.add(i, 0);
        }
        pool.reclaim(0, SPARE_KEPT, true);
        assert_eq!(pool.blocks(), STAGING_KEPT + 1);
        pool.reclaim(REST - 1, SPARE, true);
        assert_eq!(pool.blocks(), SPARE + 1);
        assert_eq!(pool.resting.len(), 1);
        pool.reclaim(REST, 0, true);
        assert_eq!(pool.blocks(), 1);
        assert!(pool.resting.is_empty());
        pool.give(held, REST);
        pool.reclaim(2 * REST, 0, true);
        assert_eq!(pool.blocks(), 0);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    /// (Fox F, `VICTORY_POOL_CLASSES`) Pieces stay in blocks of their class,
    /// take the smallest hole they fit, and past the cap go into another
    /// class's block before a new one is made.
    #[test]
    fn classes_take_the_best_hole_of_their_class() {
        let _card = CARD_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        pool.classes = Some(Cap { pct: 95 });
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let small = |pool: &mut Pool<u32>, at| pool.take(64 << 10, 4096, at, make).unwrap().0;
        let a = small(&mut pool, 0);
        let b = small(&mut pool, 0);
        let c = small(&mut pool, 0);
        let big = pool.take(16 << 20, 65536, 0, make).unwrap().0;
        assert_ne!(big.block, a.block, "a large piece does not go in the small pieces' block");
        assert_eq!((a.block, b.block, c.block), (a.block, a.block, a.block));
        // Holes of 64 and 128 KiB (b given back, then a and c joined... a
        // alone): a 64 KiB piece takes the 64 KiB hole, not the block's tail.
        pool.give(b, 0);
        pool.rested(REST);
        let d = small(&mut pool, REST);
        assert_eq!((d.block, d.start), (b.block, b.start));
        assert_eq!(pool.blocks[a.block].as_ref().unwrap().class, 0);
        // An emptied block takes any class again.
        for p in [a, c, d] {
            pool.give(p, REST);
        }
        pool.rested(2 * REST);
        let blk = pool.blocks[a.block].as_ref().unwrap();
        assert!(blk.free.whole(BLOCK));
        assert_eq!(blk.class, NO_CLASS);
        // Ranges stay consistent both ways.
        assert_eq!(blk.free.0.len(), blk.free.1.len());
        // Past the cap a piece goes into another class's block.
        CARD[0].store(BLOCK, Ordering::Relaxed);
        CARD[1].store(BLOCK, Ordering::Relaxed);
        let blocks = pool.blocks();
        let medium = pool.take(2 << 20, 65536, 2 * REST, make).unwrap().0;
        assert_eq!(pool.blocks(), blocks, "no new block past the cap while one has room");
        let _ = medium;
        CARD[0].store(0, Ordering::Relaxed);
        assert!(CLASSED[CLASS_OTHER].load(Ordering::Relaxed) + CLASSED[CLASS_EMPTY].load(Ordering::Relaxed) >= 1);
    }

    /// (Fox V, `VICTORY_POOL_DRAIN`) The sparsest blocks past what the bytes
    /// in use need get no piece while another block can take it, empty as
    /// their pieces go, and go back; with no other room they still take one
    /// before a block is made.
    #[test]
    fn drained_blocks_take_nothing_while_others_have_room_and_empty() {
        let _card = CARD_TEST.lock().unwrap_or_else(|e| e.into_inner());
        for classed in [false, true] {
            let mut pool: Pool<u32> = Pool::new(SPARE);
            if classed {
                pool.classes = Some(Cap { pct: 95 });
            } else {
                // As `cap()` with `VICTORY_POOL_DRAIN=1`.
                pool.capped = Some(Cap { pct: 95 });
            }
            let made = std::cell::Cell::new(0);
            let make = || {
                made.set(made.get() + 1);
                Some(made.get())
            };
            let mb = 1u64 << 20;
            // Four blocks of 16 x 4 MiB; then three quarters of the pieces go,
            // a few in each block: 16 + 4 + 4 + 4 pieces stay (112 MiB).
            let mut pieces = Vec::new();
            for _ in 0..64 {
                pieces.push(pool.take(4 * mb, 65536, 0, make).unwrap().0);
            }
            assert_eq!(pool.blocks(), 4);
            let mut kept = Vec::new();
            for p in pieces {
                if p.block == 0 || p.start < 16 * mb {
                    kept.push(p);
                } else {
                    pool.give(p, 0);
                }
            }
            pool.rested(REST);
            pool.drain = Some(90);
            pool.mark_draining();
            // 112 MiB need 2 blocks at 90 %: the two sparsest of the four.
            assert_eq!(DRAIN_NOW[0].load(Ordering::Relaxed), 2);
            let drained: Vec<usize> =
                pool.blocks.iter().enumerate().filter_map(|(i, b)| b.as_ref().filter(|b| b.draining).map(|_| i)).collect();
            assert_eq!(drained.len(), 2);
            assert!(!drained.contains(&0), "the full block is not drained");
            // New pieces go into the open sparse block (48 MiB of room).
            for _ in 0..12 {
                let p = pool.take(4 * mb, 65536, REST, make).unwrap().0;
                assert!(!drained.contains(&p.block));
                kept.push(p);
            }
            // No room left outside: a drained block before a new one.
            let before = DRAIN[DRAIN_FALLBACK].load(Ordering::Relaxed);
            let p = pool.take(4 * mb, 65536, REST, make).unwrap().0;
            assert!(drained.contains(&p.block));
            assert_eq!(pool.blocks(), 4, "no block made while a drained one has room");
            assert!(DRAIN[DRAIN_FALLBACK].load(Ordering::Relaxed) > before);
            pool.give(p, REST);
            // The drained blocks' pieces go: they empty and go back.
            let emptied = DRAIN[DRAIN_EMPTIED].load(Ordering::Relaxed);
            kept.retain(|p| {
                let go = drained.contains(&p.block);
                if go {
                    pool.give(*p, REST);
                }
                !go
            });
            pool.most = 0;
            pool.rested(2 * REST);
            assert_eq!(pool.blocks(), 2);
            assert!(DRAIN[DRAIN_EMPTIED].load(Ordering::Relaxed) >= emptied + 2);
            pool.mark_draining();
            assert!(pool.blocks.iter().flatten().all(|b| !b.draining), "full blocks are not drained");
        }
    }

    /// (Fox V, `VICTORY_POOL_CAP`) Past the raw cap no block is made ahead,
    /// a hole in a block holding something comes before an empty block, and
    /// a block is made only when none can take the piece (counted).
    #[test]
    fn past_the_cap_holes_come_before_empty_and_new_blocks() {
        let _card = CARD_TEST.lock().unwrap_or_else(|e| e.into_inner());
        CARD[0].store(BLOCK, Ordering::Relaxed);
        CARD[1].store(BLOCK, Ordering::Relaxed);
        CARD[2].store(0, Ordering::Relaxed);
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        pool.capped = Some(Cap { pct: 95 });
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let ripe = Ripe { started: 0, finished: 0, ahead: 2 };
        let past = CAPPED[CAP_PAST].load(Ordering::Relaxed);
        let held = CAPPED[CAP_AHEAD_HELD].load(Ordering::Relaxed);
        let (a, _, _) = pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap();
        assert_eq!(pool.blocks(), 1, "no block made ahead past the cap");
        assert!(CAPPED[CAP_PAST].load(Ordering::Relaxed) > past);
        assert!(CAPPED[CAP_AHEAD_HELD].load(Ordering::Relaxed) > held);
        let (_b, _, _) = pool.take_ripe(BLOCK / 4, 65536, 0, ripe, make).unwrap();
        pool.give(a, 0);
        pool.rested(REST);
        // An empty block at a lower place than the hole: still the hole.
        pool.blocks.insert(0, None);
        pool.add(99, 0);
        let (c, _, _) = pool.take_ripe(BLOCK / 4, 65536, REST, ripe, make).unwrap();
        assert_eq!(c.block, 1, "the hole of the block holding something");
        assert!(pool.blocks[0].as_ref().unwrap().free.whole(BLOCK));
        assert_eq!(pool.blocks(), 2);
        CARD[0].store(0, Ordering::Relaxed);
    }

    /// (Fox V) Size steps of the sizes meter.
    #[test]
    fn size_steps() {
        assert_eq!(step(4096), 0);
        assert_eq!(step(8191), 0);
        assert_eq!(step(8192), 1);
        assert_eq!(step(1 << 20), 8);
        assert_eq!(step(BLOCK), STEPS - 1);
        assert_eq!(step(0), 0);
    }

    /// (Fox F) A pool's own rest (`VICTORY_STAGING_REST_MS`), and packing
    /// staging pieces into the fullest block so that the sparse one empties.
    #[test]
    fn a_shorter_rest_and_packing_let_a_staging_block_empty() {
        let mut pool: Pool<u32> = Pool::new(STAGING_PACKED_KEPT);
        pool.rest = 250;
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let (a, _, _) = pool.take(BLOCK / 2, 512, 0, make).unwrap();
        let (b, _, _) = pool.take(BLOCK / 2, 512, 0, make).unwrap();
        let (c, _, _) = pool.take(BLOCK / 4, 512, 0, make).unwrap();
        assert_eq!((a.block, b.block), (0, 0));
        assert_eq!(c.block, 1);
        pool.give(a, 100);
        // Not before its own rest, then at once.
        let (d, _, _) = pool.take(BLOCK / 4, 512, 100 + 249, make).unwrap();
        assert_eq!(d.block, 1, "first fit: block 1 has room, block 0's half rests");
        let (e, _, again) = pool.take(BLOCK / 4, 512, 100 + 250, make).unwrap();
        assert_eq!((e.block, e.start, again), (0, 0, true));
        // Packed: the fullest block that fits takes the piece (block 1 holds
        // a half, block 0 three quarters).
        pool.pack = true;
        let (f, _, _) = pool.take(BLOCK / 8, 512, 400, make).unwrap();
        assert_eq!(f.block, 0);
        // Block 1 given back whole goes at once past the kept ones.
        pool.most = 0;
        pool.give(c, 500);
        pool.give(d, 500);
        pool.rested(750);
        assert_eq!(pool.blocks(), 1);
        assert_eq!(pool.given_back, 1);
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }
}
