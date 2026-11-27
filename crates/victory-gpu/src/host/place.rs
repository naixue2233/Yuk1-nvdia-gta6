//! The title's buffer copies, done on the card at their place in the list
//! (`VICTORY_COPIES_IN_PLACE`, on by default; `0`: all at submission, on the
//! CPU, as before).
//!
//! A buffer copy was done in title memory when its list was submitted, before
//! any draw of the submission ran, and the host copies refreshed after it.
//! That is another order than the list's: draws ahead of the copy read what
//! it put; of two copies to the same bytes with a draw behind each, both
//! draws read the second; a source that a dispatch of the same submission
//! writes first was read as it was before it; and a destination the
//! submissions in the air still read changed under them (where its host copy
//! has one side only, `crate::shadow::card_all`).
//!
//! Now the list copies, in the destination's host copy: out of the source's
//! host copy where shaders write that one, else out of upload memory of the
//! list that takes the source's bytes when the list is submitted (a source
//! shaders never wrote is the title's CPU's, or a copy's put there earlier in
//! the submission). What is written there is a shader's write from then on:
//! the bytes are declared, and what the card made of them is put into title
//! memory before the title is told of the submission.
//!
//! Title memory takes the copy at submission as well, as it did: fills of
//! upload memory, textures and host copies that lie over the same bytes read
//! it there. The reference of the destination's host copy takes the same
//! bytes ([`Shadow::placed_copy`]), so what the landing finds is what the card
//! made of them afterwards (a counter copied to zero and counted up; a source
//! that a shader wrote in between), and nothing is sent up over it. Where
//! flights in the air may write the source or the destination this is left to
//! the landing: what they wrote is not in title memory yet, and what they
//! will put there must not stand over this copy's bytes. If a step of the
//! submission reads the destination on the CPU, it waits for them instead
//! ([`Placed::waits`]).
//!
//! Copies to memory that has no host copy yet are done as before: nothing on
//! the card has read or written it. So are those of more than 8 MiB out of
//! memory shaders do not write (streaming).

use super::*;
use crate::shadow::Shadow;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPIES_IN_PLACE").is_none_or(|v| v != "0"))
}

/// The most bytes copied out of memory shaders do not write.
const MOST: usize = 8 << 20;

/// Since the last frame-rate line: copies at their place, their bytes, those
/// out of a host copy, and texture copies out of one.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the CPU does at submission for a copy at its place in a list.
pub(super) struct Placed {
    pub(super) dst: usize,
    pub(super) src: usize,
    pub(super) len: usize,
    /// The destination's host copy.
    pub(super) shadow: std::sync::Arc<Shadow>,
    /// Upload memory the list copies from, to be given the source's bytes.
    from: Option<usize>,
}

impl Placed {
    /// Whether flights that write `written` are waited for.
    pub(super) fn waits(&self, written: &[(usize, usize)]) -> Waits {
        let source = touches(written, self.src, self.src + self.len);
        if source && self.from.is_some() {
            return Waits::Yes;
        }
        match source || touches(written, self.dst, self.dst + self.len) {
            true => Waits::ForReaders,
            false => Waits::No,
        }
    }

    /// At submission, in the order of the steps: the list's upload memory
    /// takes the source's bytes and, if `free` (no flight in the air may write
    /// source or destination), title memory at the destination does. Gives
    /// the bytes if it took them.
    pub(super) fn prepare(&self, free: bool) -> Option<Vec<u8>> {
        if !free && self.from.is_none() {
            return None;
        }
        let mut bytes = vec![0u8; self.len];
        // (What is not there reads as zeros, as any copy's source.)
        copy_mapped(bytes.as_mut_ptr(), self.src as *const u8, self.len, false);
        if let Some(to) = self.from {
            // SAFETY: the list's own upload memory of that many bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to as *mut u8, self.len) };
        }
        if !free {
            return None;
        }
        copy_mapped(self.dst as *mut u8, bytes.as_ptr(), self.len, true);
        crate::watch::wrote(self.dst, self.len);
        // Title memory that is not all there was left alone.
        let mut there = vec![0u8; self.len];
        (read_into(self.dst as *const u8, &mut there) && there == bytes).then_some(bytes)
    }
}

/// Whether a copy waits for flights before its submission is prepared: not;
/// yes, as it takes a source on the CPU that they may write; or only if
/// another step of the submission reads its destination on the CPU.
pub(super) enum Waits {
    No,
    ForReaders,
    Yes,
}

/// Whether a step of `runs` reads one of `ranges` on the CPU.
pub(super) fn read_by(runs: &[&HostList], mut ranges: Vec<(usize, usize)>) -> bool {
    join_ranges(&mut ranges, []);
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            &Step::Copy(_, src, len) | &Step::Fill(_, src, len, _) => touches(&ranges, src, src + len),
            Step::Texture(staging) => {
                let (at, len) = staging.memory();
                touches(&ranges, at, at + len)
            }
            &Step::Plane((at, len), _) => touches(&ranges, at, at.saturating_add(len)),
            Step::Host(_, (from, to)) => touches(&ranges, *from, *to),
            Step::Placed(p) => p.from.is_some() && touches(&ranges, p.src, p.src + p.len),
            _ => false,
        })
    })
}

/// (Coder W, diagnostics) [`read_by`]'s first match: what reads a placed copy's destination
/// on the CPU, and where.
#[cfg(feature = "diagnostics")]
pub(super) fn read_by_who(runs: &[&HostList], mut ranges: Vec<(usize, usize)>) -> Option<(&'static str, usize, usize)> {
    join_ranges(&mut ranges, []);
    for list in runs {
        for step in list.deferred().copies.iter() {
            let hit = match step {
                &Step::Copy(_, src, len) => touches(&ranges, src, src + len).then_some(("a placed copy's destination, read by a buffer copy", src, len)),
                &Step::Fill(_, src, len, _) => touches(&ranges, src, src + len).then_some(("a placed copy's destination, read by a fill", src, len)),
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    touches(&ranges, at, at + len).then_some(("a placed copy's destination, read by a texture", at, len))
                }
                &Step::Plane((at, len), _) => touches(&ranges, at, at.saturating_add(len)).then_some(("a placed copy's destination, read by a plane", at, len)),
                Step::Host(_, (from, to)) => touches(&ranges, *from, *to).then_some(("a placed copy's destination, read by a host step", *from, to - from)),
                Step::Placed(p) => (p.from.is_some() && touches(&ranges, p.src, p.src + p.len))
                    .then_some(("a placed copy's destination, the source of a placed copy from the CPU", p.src, p.len)),
                _ => None,
            };
            if hit.is_some() {
                return hit;
            }
        }
    }
    None
}

/// Everything recorded so far is done, and seen, before what follows.
///
/// # Safety
/// An open list.
pub(super) unsafe fn wait_for_all(host: &Host, list: &ID3D12GraphicsCommandList) {
    static ENHANCED: OnceLock<bool> = OnceLock::new();
    let enhanced = *ENHANCED.get_or_init(|| {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
        // SAFETY: a feature structure of its size.
        let known = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS12,
                (&raw mut o).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS12>() as u32,
            )
        };
        known.is_ok() && o.EnhancedBarriersSupported.as_bool()
    });
    // SAFETY: the caller's.
    unsafe {
        if enhanced && let Ok(list) = list.cast::<ID3D12GraphicsCommandList7>() {
            let all = [D3D12_GLOBAL_BARRIER {
                SyncBefore: D3D12_BARRIER_SYNC_ALL,
                SyncAfter: D3D12_BARRIER_SYNC_ALL,
                AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
                AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
            }];
            list.Barrier(&[D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_GLOBAL,
                NumBarriers: 1,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pGlobalBarriers: all.as_ptr(),
                },
            }]);
            return;
        }
        let alias = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    pResourceAfter: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        let uav = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        list.ResourceBarrier(&[alias, uav]);
    }
}

/// The host copy of `[src, src + len)` if shaders write it: what it holds at
/// a place in a list is not what title memory holds at submission.
fn shaders_own(src: usize, len: usize) -> Option<std::sync::Arc<Shadow>> {
    crate::shadow::peek(src as u64, len as u64).filter(|s| s.written() && !s.in_place())
}

impl HostList {
    /// Copies `len` bytes of title memory at `src` to `dst`, at this place of
    /// the list (see the file). `false` if it is to be done at submission.
    pub(crate) fn place_copy(&self, dst: usize, src: usize, len: usize) -> bool {
        if !on() {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let Some(to) = crate::shadow::peek(dst as u64, len as u64).filter(|s| !s.in_place()) else {
            return false;
        };
        let own = shaders_own(src, len);
        let scratch = match &own {
            Some(_) => None,
            None if len <= MOST => match self.scratch_at(len as u64) {
                Some(scratch) => Some(scratch),
                None => return false,
            },
            None => return false,
        };
        let at = to.offset(dst as u64);
        // What the title's CPU wrote around and under it so far goes first.
        to.refresh(self, dst as u64, len as u64);
        // SAFETY (both): an open list; live buffers (the list holds the
        // copies and its scratch memory), ranges inside them.
        unsafe { wait_for_all(host, &self.list) };
        match (&own, &scratch) {
            (Some(from), _) => {
                from.refresh(self, src as u64, len as u64);
                let offset = from.offset(src as u64);
                if from.buffer.as_raw() == to.buffer.as_raw() {
                    let Some(between) = self.between(host, len as u64) else {
                        return false;
                    };
                    unsafe {
                        self.list.CopyBufferRegion(&between, 0, &from.buffer, offset, len as u64);
                        self.list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                        wait_for_all(host, &self.list);
                        self.list.CopyBufferRegion(&to.buffer, at, &between, 0, len as u64);
                    }
                } else {
                    unsafe { self.list.CopyBufferRegion(&to.buffer, at, &from.buffer, offset, len as u64) };
                }
                STATS[2].fetch_add(1, Ordering::Relaxed);
            }
            (None, Some((from, offset, _))) => unsafe {
                self.list.CopyBufferRegion(&to.buffer, at, from, *offset, len as u64);
            },
            (None, None) => return false,
        }
        unsafe { wait_for_all(host, &self.list) };
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(len as u64, Ordering::Relaxed);
        to.gpu_writes(self, dst as u64, len as u64);
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Placed(Placed {
            dst,
            src,
            len,
            shadow: to,
            from: scratch.map(|s| s.2 as usize),
        }));
        deferred.title_written();
        self.changed_something();
        true
    }

    /// For a copy of `[src, src + len)` into a texture: a buffer (readable by
    /// copies) that holds what shaders wrote there, at this place of the list.
    /// `None` where shaders do not write it.
    pub(crate) fn place_source(&self, src: usize, len: usize) -> Option<ID3D12Resource> {
        if !on() {
            return None;
        }
        let host = host()?;
        let from = shaders_own(src, len)?;
        let between = self.between(host, len as u64)?;
        from.refresh(self, src as u64, len as u64);
        STATS[3].fetch_add(1, Ordering::Relaxed);
        // SAFETY: an open list; live buffers, ranges inside them.
        unsafe {
            wait_for_all(host, &self.list);
            self.list.CopyBufferRegion(&between, 0, &from.buffer, from.offset(src as u64), len as u64);
            self.list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            wait_for_all(host, &self.list);
        }
        Some(between)
    }

    /// A buffer on the card for this recording alone.
    fn between(&self, host: &Host, len: u64) -> Option<ID3D12Resource> {
        let buffer = host.uav_buffer(len.next_multiple_of(1 << 16))?;
        name(&buffer, || "bytes between copies".into());
        self.keep(&buffer);
        Some(buffer)
    }
}

#[cfg(test)]
mod tests {
    /// What copies with two sides cost to make (ignored: a measurement for
    /// `VICTORY_CARD_ALL`): buffers of 64 KiB on the card and in the PC's
    /// memory.
    #[test]
    #[ignore]
    fn small_buffers_cost() {
        let Some(host) = super::host() else {
            return;
        };
        const N: usize = 1000;
        let used = || host.video_memory().map_or(0, |m| m[1] >> 20);
        let before = used();
        let t = std::time::Instant::now();
        let card: Vec<_> = (0..N).map(|_| host.uav_buffer(1 << 16)).collect();
        let card_us = t.elapsed().as_micros() as f64 / N as f64;
        let after = used();
        let t = std::time::Instant::now();
        let pc: Vec<_> = (0..N).map(|_| host.shared_buffer(1 << 16)).collect();
        let pc_us = t.elapsed().as_micros() as f64 / N as f64;
        println!(
            "{N} buffers of 64 KiB: on the card {card_us:.0} us each, {} MB of its memory; in the PC's memory {pc_us:.0} us each",
            after.saturating_sub(before)
        );
        drop((card, pc));
    }
}

/// (Coder W) `VICTORY_FILL_FROM_PLACED`: 0 off, 1 on, 2 verify.
///
/// A fill of upload memory that reads the destination of a copy at its place earlier in the
/// submission waited for every flight that may write that destination or the copy's source
/// ([`Waits::ForReaders`], `blocked_by`): run-192 had 3-6 ms a frame of such waits, all fills
/// of a few hundred bytes behind copies of the same size. Where the copy takes its source on
/// the CPU (`from`: no flight writes the source), the bytes the destination holds right after
/// the copy are the source's as the copy reads them at submission, whatever the flights put
/// there before. So a fill that reads only bytes that copy wrote, with no step between them
/// writing those bytes or the copy's source (a buffer copy, another copy at its place, a step
/// of ours ends the look), reads the copy's source instead: the same bytes, no wait. With
/// `verify` the fills are left as they are (the wait stands) and, once they are done, the
/// bytes they read are compared with what the other source holds: differing bytes are
/// counted (must be 0).
pub(super) fn fill_from_placed() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_FILL_FROM_PLACED").as_deref().map(str::trim) {
        Ok("1") => 1,
        Ok("verify") => 2,
        _ => 0,
    })
}

/// [`fill_from_placed`]'s counters since the start: fills redirected (or found redirectable),
/// their bytes, fills compared, bytes that differed.
static FROM_PLACED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

thread_local! {
    /// (verify) The fills to compare once done: (what they read, the copy's source, bytes).
    static TO_CHECK: std::cell::RefCell<Vec<(usize, usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Before the submission of `runs` is planned, with flights in the air writing `written`.
pub(super) fn redirect_fills(runs: &[&HostList], written: &[(usize, usize)]) {
    use std::sync::atomic::Ordering::Relaxed;
    let mode = fill_from_placed();
    if mode == 0 || written.is_empty() {
        return;
    }
    // Copies at their place seen so far whose bytes are known at submission: (dst, src, len).
    let mut known: Vec<(usize, usize, usize)> = Vec::new();
    let meets = |a: usize, b: usize, c: usize, d: usize| a < d && c < b;
    for list in runs {
        let mut deferred = list.deferred();
        for step in deferred.copies.iter_mut() {
            // What title memory this step writes, if any.
            let wrote = match step {
                Step::Run(_) => {
                    known.clear();
                    continue;
                }
                &mut Step::Copy(dst, _, len) => Some((dst, len)),
                Step::Placed(p) => {
                    let w = (p.dst, p.len);
                    // (A newer one over the same bytes stands from here.)
                    known.retain(|&(d, s, l)| !meets(d, d + l, w.0, w.0 + w.1) && !meets(s, s + l, w.0, w.0 + w.1));
                    if p.from.is_some() && !matches!(p.waits(written), Waits::Yes) {
                        known.push((p.dst, p.src, p.len));
                    }
                    continue;
                }
                Step::Fill(_, src, len, _) => {
                    let (at, n) = (*src, *len);
                    if let Some(&(d, s, _)) = known.iter().rev().find(|&&(d, _, l)| d <= at && at + n <= d + l) {
                        let from = s + (at - d);
                        FROM_PLACED[0].fetch_add(1, Relaxed);
                        FROM_PLACED[1].fetch_add(n as u64, Relaxed);
                        if mode == 1 {
                            *src = from;
                        } else {
                            TO_CHECK.with_borrow_mut(|c| c.push((at, from, n)));
                        }
                    }
                    None
                }
                _ => None,
            };
            if let Some((w, n)) = wrote {
                known.retain(|&(d, s, l)| !meets(d, d + l, w, w + n) && !meets(s, s + l, w, w + n));
            }
        }
    }
}

/// (verify) After the submission's fills are done: what they read against the copies' sources.
pub(super) fn check_fills() {
    use std::sync::atomic::Ordering::Relaxed;
    let checks = TO_CHECK.with_borrow_mut(std::mem::take);
    for (read, other, n) in checks {
        let (mut a, mut b) = (vec![0u8; n], vec![0u8; n]);
        let _ = (read_into(read as *const u8, &mut a), read_into(other as *const u8, &mut b));
        FROM_PLACED[2].fetch_add(1, Relaxed);
        let differ = a.iter().zip(&b).filter(|(x, y)| x != y).count() as u64;
        if differ > 0 {
            FROM_PLACED[3].fetch_add(differ, Relaxed);
            tracing::warn!(at = format_args!("{read:#x}"), other = format_args!("{other:#x}"), n, differ, "fill from a placed copy's source: bytes differ");
        }
    }
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_none_or(|t| t.elapsed().as_secs() >= 5) {
        *last = Some(std::time::Instant::now());
        tracing::info!(
            "fills from a placed copy's source (VICTORY_FILL_FROM_PLACED={}), since the start: {} fills {} bytes {}; compared {}; MUST BE 0: bytes that differ {}",
            ["0", "1", "verify"][fill_from_placed() as usize],
            FROM_PLACED[0].load(Relaxed),
            FROM_PLACED[1].load(Relaxed),
            if fill_from_placed() == 1 { "read from the source instead (no wait)" } else { "would be" },
            FROM_PLACED[2].load(Relaxed),
            FROM_PLACED[3].load(Relaxed),
        );
    }
}
