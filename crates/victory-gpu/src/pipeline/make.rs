//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline, TargetFormats};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]) or a confirmed
    /// opening-scene / bake producer that must never sit behind cold RT shaders.
    screen: bool,
    /// Confirmed first-scene / bake producer prepared early even on a cold cache.
    scene_input: bool,
    /// Why this pipeline is intentionally skipped (e.g. RT resolve without BVH).
    left_out: Option<&'static str>,
    /// (Fox P) Which of the programs left out by name ([`FAMILIES`]) it is
    /// one of, made or not: for the per-frame counters.
    pub(crate) family: Option<usize>,
    /// (Fox P, diagnostics) When it was first queued, its draws and
    /// dispatches dropped since the last report, and whether the report
    /// already follows it ([`note_dropped`]).
    queued_at: OnceLock<Instant>,
    drops: AtomicU64,
    followed: AtomicBool,
    /// (fxceless a35c557) The formats its state declares for what a draw draws into
    /// (zeros without a graphics state), and the same shaders and state for other formats
    /// bound at a draw ([`for_formats`]): a chain, one link per set of formats met.
    pub(crate) formats: TargetFormats,
    variant: OnceLock<Arc<Pending>>,
    /// (Fox T, diagnostics) Its programs' names for the pass times
    /// (`VICTORY_PASS_TIMES`), made on first use ([`Pending::label`]).
    #[cfg(feature = "diagnostics")]
    label: OnceLock<Arc<str>>,
}

fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders
        .iter()
        .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
}

/// (Fox P) The programs [`left_out`] leaves out by name, the only such list in
/// this tree: one per material (`billboard_nobump:CS_RaytracingResolve_*`,
/// `cloth_default:...`; ~1,000) that shades the hits of the title's rays, and
/// the screen probes' program that activates their rays.
pub(crate) const FAMILIES: [&str; 2] = ["CS_RaytracingResolve", "CS_ScreenProbes_ActivateRays"];

/// (Fox P, diagnostics) Per family: dispatches run, dropped; indirect ones
/// run, dropped; since the last report.
#[cfg(feature = "diagnostics")]
pub(crate) static FAMILY_RUNS: [[AtomicU64; 4]; 2] = [const { [const { AtomicU64::new(0) }; 4] }; 2];

fn family(shaders: &[Arc<Shader>]) -> Option<usize> {
    FAMILIES.iter().position(|name| named(shaders, name.as_bytes()))
}

fn leave_out() -> &'static [String] {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    HASHES.get_or_init(|| {
        std::env::var("VICTORY_LEAVE_OUT")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect()
    })
}

fn left_out(shaders: &[Arc<Shader>], family: Option<usize>) -> Option<&'static str> {
    if !leave_out().is_empty() && shaders.iter().any(|s| leave_out().contains(&s.hash)) {
        return Some("named in VICTORY_LEAVE_OUT, left out");
    }
    static SKIP_RT_RESOLVE: OnceLock<bool> = OnceLock::new();
    let skip_rt = *SKIP_RT_RESOLVE.get_or_init(|| {
        let builder = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
        // Without BVH, ~1,000 CS_RaytracingResolve shaders shade no hits while
        // clogging the cold compiler queue; skip unless VICTORY_RT_RESOLVE=1.
        let skip = !builder && std::env::var_os("VICTORY_RT_RESOLVE").is_none_or(|v| v == "0");
        tracing::info!(
            left_out = skip,
            builder,
            "programs left out by name: CS_RaytracingResolve (shades the hits of the title's rays, one per material) \
             is left out while the ray-tracing structure builder is off (VICTORY_BVH_BUILDER=1 or VICTORY_RT_RESOLVE=1: made); \
             its dispatches count as 'left out by name', not 'being made'"
        );
        skip
    });
    if skip_rt && family == Some(0) {
        return Some("shades ray-tracing hits without BVH, left out (VICTORY_RT_RESOLVE=1: made)");
    }
    static SKIP_ACTIVATE_RAYS: OnceLock<bool> = OnceLock::new();
    let skip_activate = *SKIP_ACTIVATE_RAYS.get_or_init(|| {
        let builder = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
        if builder {
            return std::env::var_os("VICTORY_ACTIVATE_RAYS").is_some_and(|v| v == "0");
        }
        // (Fox T) `VICTORY_PROBE_RAYS=1`: made, against empty structures.
        if victory_shader::emit::probe_rays() {
            return false;
        }
        std::env::var_os("VICTORY_ACTIVATE_RAYS").is_none_or(|v| v == "0")
    });
    if skip_activate && family == Some(1) {
        return Some("ray activation shader without BVH, left out (VICTORY_ACTIVATE_RAYS=1: made)");
    }
    None
}

/// Confirmed first-scene producers. Prepare these at creation even on a cold
/// launch so cutscene lighting, tiling, and bakes never miss their first frame.
fn scene_input(shaders: &[Arc<Shader>]) -> bool {
    shaders.iter().any(|s| {
        matches!(
            s.hash.as_str(),
            "970c4fbb3b017c0e"
                | "068c90badc86f1fd"
                | "e4a7e088c7780ce8"
                | "ebe4e53f031f5249"
                | "2cc4c197ee29e291"
                | "369cbe906ea72764"
        ) || crate::draw::BAKE_PIXEL_SHADERS.contains(&s.hash.as_str())
    })
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        let scene_input = scene_input(&shaders);
        let screen = (graphics.as_ref().is_some_and(|g| g.screen_pass())
            && shaders.iter().all(|s| s.stage != "mesh"))
            || scene_input;
        let family = family(&shaders);
        let left_out = left_out(&shaders, family);
        Pending {
            family,
            queued_at: OnceLock::new(),
            drops: AtomicU64::new(0),
            followed: AtomicBool::new(false),
            formats: graphics.as_ref().map_or([0; 9], |g| g.formats()),
            variant: OnceLock::new(),
            screen,
            scene_input,
            left_out,
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            #[cfg(feature = "diagnostics")]
            label: OnceLock::new(),
        }
    }

    /// (Fox T, diagnostics) What the pass times call it: each stage's
    /// program name as its container carries it (`group:CS_Name_hash`), or
    /// the stage and hash where it carries none; `builder` puts
    /// `bvh-builder` in front (the console's structure builder's stages).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn label(&self, builder: bool) -> Arc<str> {
        self.label
            .get_or_init(|| {
                let parts: Vec<String> = self
                    .shaders
                    .iter()
                    .map(|s| program_name(&s.container).unwrap_or_else(|| format!("{} {}", s.stage, s.hash)))
                    .collect();
                let text = parts.join(" + ");
                Arc::from(if builder { format!("bvh-builder {text}") } else { text })
            })
            .clone()
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let began = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            // (Fox P) A creation that fails is final: `made` holds `None`
            // (the reason was logged by `HostPipeline::new`), never "being made".
            if made.is_none() {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }
            MADE_RECENT.fetch_add(1, Ordering::Relaxed);
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    // (Fox P) A pipeline left out by name is never queued and never made: it
    // was counted as "being made" for whole runs (~600 dispatches a frame).
    if let Some(why) = p.left_out {
        return Some(format!("left out by name: {}", FAMILIES[p.family.unwrap_or(0)]) + ", " + why);
    }
    match p.made.get() {
        Some(Some(_)) => None,
        // (Fox P) Never queued: the draw went to a variant for its bound
        // formats (`for_formats`), which is what is not there.
        None if p.queue_state.load(Ordering::Relaxed) == NOT_QUEUED => {
            Some("its variant for the bound render target formats is not there (VICTORY_TARGET_VARIANTS)".into())
        }
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// (fxceless a35c557) Pipelines made again for the formats bound at a draw, since the start.
pub(crate) static VARIANTS: AtomicU64 = AtomicU64::new(0);

/// Whether a draw into views of other formats than its pipeline declares gets a host
/// pipeline for those formats ([`for_formats`]); `VICTORY_TARGET_VARIANTS=0` sends such a
/// draw with the title's pipeline, as before.
fn variants_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TARGET_VARIANTS").is_none_or(|v| v != "0"))
}

/// (fxceless a35c557) The host pipeline for a draw with `p` into views of the formats
/// `bound`, which `p` does not declare (`host::formats_differ`): that of the same shaders
/// and state for those formats, a pipeline like the title's own (queued at its first draw,
/// the draw dropped while it is not there, [`ready`]).
#[cold]
pub(crate) fn for_formats<'a>(p: &'a Arc<Pending>, bound: &TargetFormats) -> Option<&'a HostPipeline> {
    match p.graphics.as_deref() {
        Some(g) if variants_on() => ready(variant(p, g, &super::host::formats_for(&p.formats, bound))),
        _ => ready(p),
    }
}

/// The pipeline of `p`'s shaders and its state `g` for the formats `bound`: found in the
/// chain that starts at `p`, or added to it (logged then, once per pipeline and formats).
fn variant<'a>(p: &'a Arc<Pending>, g: &GraphicsState, bound: &TargetFormats) -> &'a Arc<Pending> {
    let mut at = p;
    for _ in 0..64 {
        let next = at.variant.get_or_init(|| {
            let so_far = VARIANTS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                pipeline = ?p.shaders.iter().map(|s| s.hash.as_str()).collect::<Vec<_>>(),
                declared = ?p.formats,
                bound = ?bound,
                so_far,
                "a pipeline is made again for the render target formats bound at its draw (eight render targets, then depth; VICTORY_TARGET_VARIANTS=0: the draw goes out with the title's pipeline)"
            );
            Arc::new(Pending::new(p.shaders.clone(), Some(Box::new(g.with_formats(bound)))))
        });
        if next.formats == *bound {
            return next;
        }
        at = next;
    }
    p
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.left_out.is_some() {
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// or if it is a confirmed opening-scene / bake producer.
pub(crate) fn early(p: &Arc<Pending>) {
    if p.left_out.is_some() {
        return;
    }
    if !p.shaders.is_empty() && (p.scene_input || super::library::wanted_early(&p.key)) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, if p.scene_input { REQUESTED } else { EARLY });
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let screen = p.screen && screens_first();
    let budget = if screen { screen_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
    crate::framelog::add(crate::framelog::T::PipelineWaitUs, started.elapsed().as_micros() as u64);
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
        }
    })
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
    /// (Fox P, diagnostics) Every pipeline queued and not seen made yet by
    /// [`report`].
    #[cfg(feature = "diagnostics")]
    all: Vec<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            #[cfg(feature = "diagnostics")]
            all: Vec::new(),
            screen: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else if slow {
            self.slow.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        let mut p = self.screen.pop_front();
        if p.is_none() && !screens_only {
            p = self.requested.pop_front().or_else(|| unclaimed(&mut self.early));
            if p.is_none() && self.slow_now < slow_limit {
                p = unclaimed(&mut self.slow);
                if let Some(p) = &p {
                    p.was_slow.store(true, Ordering::Relaxed);
                    self.slow_now += 1;
                }
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                let _ = p.queued_at.set(Instant::now());
                queue.all.push(p.clone());
            }
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit()) {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    worker_count_for(asked, processors, false)
}

fn worker_count_for(asked: Option<usize>, processors: usize, limited_ram: bool) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None if limited_ram => (processors * 3 / 4).clamp(2, 8),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count_for(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
            crate::residency::limited_host_ram(),
        );
        tracing::info!(workers, "pipeline workers");
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        WORKERS.store((workers + screens) as u64, Ordering::Relaxed);
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                        let _ = SetThreadPriority(GetCurrentThread(), priority);
                    }
                    loop {
                        let p = next(screens_only);
                        BUSY.fetch_add(1, Ordering::Relaxed);
                        // (Fox P) A creation that panics (translator or
                        // driver wrapper) would end this worker and leave the
                        // pipeline "being made" for good: it is made final
                        // without a host pipeline instead, and said once.
                        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.make()));
                        if made.is_err() {
                            FAILED.fetch_add(1, Ordering::Relaxed);
                            tracing::error!(shaders = ?p.shaders, "a host pipeline's creation panicked: it goes without one");
                            let _ = p.made.set(None);
                            let (lock, done) = &DONE;
                            let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
                            done.notify_all();
                        }
                        BUSY.fetch_sub(1, Ordering::Relaxed);
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// (Fox P) Creations that ended without a host pipeline since the start;
/// creations ended since the last [`report`]; workers making one now, of all.
static FAILED: AtomicU64 = AtomicU64::new(0);
static MADE_RECENT: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);
static WORKERS: AtomicU64 = AtomicU64::new(0);

/// (Fox P, diagnostics) Pipelines whose draws or dispatches were dropped,
/// followed by [`report`] until they are made and drop nothing.
#[cfg(feature = "diagnostics")]
static DROPPERS: Mutex<Vec<Arc<Pending>>> = Mutex::new(Vec::new());

/// (Fox P, diagnostics) Counts a draw or dispatch of `p` dropped for want of
/// its host pipeline.
#[cfg(feature = "diagnostics")]
pub(crate) fn note_dropped(p: &Arc<Pending>) {
    p.drops.fetch_add(1, Ordering::Relaxed);
    if !p.followed.load(Ordering::Relaxed) && !p.followed.swap(true, Ordering::Relaxed) {
        DROPPERS.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
    }
}

/// A program's name as its container carries it (`..._Wrapped`), if any.
#[cfg(feature = "diagnostics")]
fn program_name(container: &[u8]) -> Option<String> {
    const END: &[u8] = b"_Wrapped";
    let at = container.windows(END.len()).position(|w| w == END)?;
    let start = container[..at]
        .iter()
        .rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b':'))
        .map_or(0, |i| i + 1);
    (at - start >= 4).then(|| String::from_utf8_lossy(&container[start..at]).into_owned())
}

/// What `p` is made of, for the report: stage, hash and name if known.
#[cfg(feature = "diagnostics")]
fn describe(p: &Pending) -> String {
    let parts: Vec<String> = p
        .shaders
        .iter()
        .map(|s| match program_name(&s.container) {
            Some(name) => format!("{} {} {name}", s.stage, s.hash),
            None => format!("{} {}", s.stage, s.hash),
        })
        .collect();
    parts.join(" + ")
}

/// Where `p` stands, for the report.
#[cfg(feature = "diagnostics")]
fn state(p: &Pending) -> String {
    if let Some(why) = p.left_out {
        return format!("left out by name ({why})");
    }
    match p.made.get() {
        Some(Some(_)) => "made".into(),
        Some(None) => format!("failed: {}", missing(p).unwrap_or_default()),
        None => match p.queue_state.load(Ordering::Relaxed) {
            NOT_QUEUED => "never queued".into(),
            EARLY => "queued early (no draw asked yet)".into(),
            REQUESTED => "queued, asked for by a draw".into(),
            _ => "compiling".into(),
        },
    }
}

/// (Fox P, diagnostics) The line on pipelines not there, for the frame
/// lines (every few seconds, over `frames` frames): pending ones, failed
/// ones, made of late, the queue and the workers; the ten pipelines whose
/// draws and dispatches were dropped most; and the programs left out by name.
#[cfg(feature = "diagnostics")]
pub(crate) fn report(frames: f64) {
    let frames = frames.max(1.0);
    let now = Instant::now();
    let age = |p: &Pending| p.queued_at.get().map_or(0.0, |t| now.duration_since(*t).as_secs_f64());
    let (pending, requested, oldest, lengths) = {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.all.retain(|p| p.made.get().is_none());
        let requested = queue.all.iter().filter(|p| p.queue_state.load(Ordering::Relaxed) >= REQUESTED).count();
        let oldest = queue.all.iter().map(|p| age(p)).fold(0.0, f64::max);
        let lengths = [queue.screen.len(), queue.requested.len(), queue.early.len(), queue.slow.len()];
        (queue.all.len(), requested, oldest, lengths)
    };
    let mut top: Vec<(u64, Arc<Pending>)> = {
        let mut droppers = DROPPERS.lock().unwrap_or_else(|e| e.into_inner());
        let mut top = Vec::new();
        droppers.retain(|p| {
            let n = p.drops.swap(0, Ordering::Relaxed);
            if n > 0 {
                top.push((n, p.clone()));
            }
            let keep = n > 0 || !matches!(p.made.get(), Some(Some(_)));
            if !keep {
                p.followed.store(false, Ordering::Relaxed);
            }
            keep
        });
        top
    };
    top.sort_by(|a, b| b.0.cmp(&a.0));
    let distinct = top.len();
    let total: u64 = top.iter().map(|t| t.0).sum();
    let shown: Vec<String> = top
        .iter()
        .take(10)
        .map(|(n, p)| {
            let pending_for = if p.made.get().is_none() && p.left_out.is_none() {
                format!(", pending for {:.0} s", age(p))
            } else {
                String::new()
            };
            format!("[{} x {:.1}/frame, {}{pending_for}]", describe(p), *n as f64 / frames, state(p))
        })
        .collect();
    tracing::info!(
        "pipelines pending: {pending} distinct ({requested} asked for by a draw; oldest {oldest:.0} s), failed since the start: {}, \
         made in the last {:.1} frames: {}, queue: screen {} asked {} early {} slow {}, workers busy {} of {}; \
         draws and dispatches dropped per frame for want of a host pipeline: {:.1} of {distinct} pipelines, the most: {}",
        FAILED.load(Ordering::Relaxed),
        frames,
        MADE_RECENT.swap(0, Ordering::Relaxed),
        lengths[0],
        lengths[1],
        lengths[2],
        lengths[3],
        BUSY.load(Ordering::Relaxed),
        WORKERS.load(Ordering::Relaxed),
        total as f64 / frames,
        if shown.is_empty() { "none".to_owned() } else { shown.join(" ") }
    );
    let families: Vec<String> = FAMILIES
        .iter()
        .zip(&FAMILY_RUNS)
        .map(|(name, runs)| {
            let [a, b, c, d] = [0, 1, 2, 3].map(|i| runs[i].swap(0, Ordering::Relaxed) as f64 / frames);
            format!("{name}: dispatches run {a:.1} dropped {b:.1}, indirect run {c:.1} dropped {d:.1}")
        })
        .collect();
    tracing::info!(
        "programs left out by name, per frame: {} (VICTORY_RT_RESOLVE=1 / VICTORY_ACTIVATE_RAYS=1: made; both made with VICTORY_BVH_BUILDER=1 \
         unless =0)",
        families.join("; ")
    );
}

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
        assert_eq!(worker_count_for(None, 8, true), 6);
        assert_eq!(worker_count_for(None, 16, true), 8);
        assert_eq!(worker_count_for(None, 24, true), 8);
        assert_eq!(worker_count_for(Some(12), 8, true), 12);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// (Fox P) A pipeline left out by name is told as such, never as
    /// "being made"; one never queued names the variant.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn left_out_pipelines_are_not_being_made() {
        let mut p = Pending::new(Vec::new(), None);
        p.left_out = Some("test");
        p.family = Some(0);
        let p = Arc::new(p);
        assert!(ready(&p).is_none());
        let why = missing(&p).unwrap();
        assert!(why.starts_with("left out by name: CS_RaytracingResolve"), "{why}");
        let q = Pending::new(Vec::new(), None);
        assert!(missing(&q).unwrap().contains("variant"));
        q.queue_state.store(MAKING, Ordering::Relaxed);
        assert_eq!(missing(&q).unwrap(), "being made");
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn program_names_are_read_from_the_container() {
        assert_eq!(
            program_name(b"\0\x01billboard_nobump:CS_RaytracingResolve_a56bfb9c_Wrapped\0").as_deref(),
            Some("billboard_nobump:CS_RaytracingResolve_a56bfb9c")
        );
        assert_eq!(program_name(b"nothing here"), None);
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }
}
