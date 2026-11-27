//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard drives one
//! stand-in gamepad (`FALLBACK_BUTTONS`). The keyboard is a device of its own
//! too. A focused window also has a mouse. A poll thread records changes
//! and runs device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const MOUSE_KIND: u32 = 0x20;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, keyboard and mouse.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const MOUSE: usize = PADS + 2;
const DEVICES: usize = MOUSE + 1;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// The keyboard's stand-in gamepad (virtual key, button); README lists it.
/// Sticks: W/A/S/D left, I/J/K/L right; triggers Z and C.
const FALLBACK_BUTTONS: [(u8, u32); 14] = [
    (0x1B, MENU), // Esc
    (0x09, VIEW), // Tab
    (0x0D, A),    // Enter
    (0x08, B),    // Backspace
    (b'X', X),
    (b'Y', Y),
    (0x26, UP), // arrows
    (0x28, DOWN),
    (0x25, LEFT),
    (0x27, RIGHT),
    (b'Q', LEFT_SHOULDER),
    (b'E', RIGHT_SHOULDER),
    (b'F', LEFT_STICK),
    (b'H', RIGHT_STICK),
];

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// GameInput v0 reports cumulative relative motion. Scroll axes stay at zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MouseState {
    buttons: u32,
    position_x: i64,
    position_y: i64,
    wheel_x: i64,
    wheel_y: i64,
}
const _: () = assert!(size_of::<MouseState>() == 40);

/// The next cursor reference and the motion since the previous focused sample.
#[cfg(any(windows, test))]
fn mouse_delta(
    previous: Option<(i32, i32)>,
    position: Option<(i32, i32)>,
    size: (i32, i32),
) -> (Option<(i32, i32)>, (i64, i64)) {
    let current = position
        .filter(|_| size.0 > 0 && size.1 > 0)
        .map(|(x, y)| (x.clamp(0, size.0 - 1), y.clamp(0, size.1 - 1)));
    let delta = match (previous, current) {
        (Some((x, y)), Some((nx, ny))) => {
            (i64::from(nx) - i64::from(x), i64::from(ny) - i64::from(y))
        }
        _ => (0, 0),
    };
    (current, delta)
}

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    fn mouse(&mut self) -> Option<MouseState> {
        None
    }
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

fn fallback(keys: &[KeyState]) -> PadState {
    let on = |vk: u8| {
        if keys.iter().any(|k| k.virtual_key == vk) {
            1.0
        } else {
            0.0
        }
    };
    PadState {
        buttons: FALLBACK_BUTTONS
            .iter()
            .filter(|(vk, _)| on(*vk) != 0.0)
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: on(b'Z'),
        right_trigger: on(b'C'),
        left_x: on(b'D') - on(b'A'),
        left_y: on(b'W') - on(b'S'),
        right_x: on(b'L') - on(b'J'),
        right_y: on(b'I') - on(b'K'),
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    match index {
        KEYBOARD => KEYBOARD_KIND,
        MOUSE => MOUSE_KIND,
        _ => GAMEPAD_KIND,
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
    Mouse(MouseState),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
            Value::Mouse(_) => MOUSE_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// `VICTORY_AUTO_A=<seconds>` (from Neptune): for that long after the start,
/// Enter (the stand-in pad's A) is down for a quarter of a second in every
/// four, whether or not the window has focus, so an unattended launch gets
/// through the title's "press A" screens. 0 (the default) is off.
static AUTO_A: LazyLock<u64> = LazyLock::new(|| {
    let seconds = std::env::var("VICTORY_AUTO_A")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if seconds > 0 {
        LazyLock::force(&START);
        tracing::info!("auto A: pressing A for the first {seconds} s");
    }
    seconds
});

/// Whether the auto press holds A `elapsed` after the start.
fn auto_a_pressed(elapsed: Duration, seconds: u64) -> bool {
    elapsed.as_secs() < seconds && elapsed.as_millis() % 4000 < 250
}

/// How long a scripted tap holds its button.
const TAP_MS: u64 = 100;

/// How long a screen condition must hold before its wait ends.
const HOLD_MS: u64 = 500;

/// How long a wait on the screen may take before the script gives up.
const GIVE_UP_MS: u64 = 180_000;

/// A button by its pad name or its stand-in key's name.
fn button_named(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" | "enter" => A,
        "b" | "backspace" => B,
        "x" => X,
        "y" => Y,
        "up" | "arrowup" => UP,
        "down" | "arrowdown" => DOWN,
        "left" | "arrowleft" => LEFT,
        "right" | "arrowright" => RIGHT,
        "start" | "menu" | "esc" | "escape" => MENU,
        "back" | "view" | "tab" => VIEW,
        "lb" => LEFT_SHOULDER,
        "rb" => RIGHT_SHOULDER,
        _ => return None,
    })
}

/// Seconds as milliseconds; `None` unless finite and not negative.
fn millis(seconds: &str) -> Option<u64> {
    seconds
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * 1000.0).round() as u64)
}

/// A condition on the most recent presented frame's draw count:
/// `d>=N`, `d>N`, `d<=N`, `d<N`.
#[derive(Clone, Debug, PartialEq)]
struct Cond {
    text: String,
    op: Op,
    n: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
}

impl Cond {
    fn parse(text: &str) -> Option<Cond> {
        let rest = text.strip_prefix(['d', 'D'])?;
        let (op, n) = if let Some(n) = rest.strip_prefix(">=") {
            (Op::Ge, n)
        } else if let Some(n) = rest.strip_prefix("<=") {
            (Op::Le, n)
        } else if let Some(n) = rest.strip_prefix('>') {
            (Op::Gt, n)
        } else if let Some(n) = rest.strip_prefix('<') {
            (Op::Lt, n)
        } else {
            return None;
        };
        let n = n.trim().parse().ok()?;
        Some(Cond { text: text.to_owned(), op, n })
    }

    fn holds(&self, draws: u32) -> bool {
        match self.op {
            Op::Ge => draws >= self.n,
            Op::Gt => draws > self.n,
            Op::Le => draws <= self.n,
            Op::Lt => draws < self.n,
        }
    }
}

/// One step of a `VICTORY_AUTO_INPUT` script.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    /// `@<seconds>`: until that long after the start (a no-op once past).
    At(u64),
    /// `w<seconds>`.
    Wait(u64),
    /// A button tapped for `TAP_MS`.
    Tap(u32),
    /// `d>=N` and the like: until the condition has held for `HOLD_MS`.
    Until(Cond),
    /// `rep(<button>,<seconds>,<cond>)`: the button tapped every period until
    /// the condition holds (looked at before each tap), then held for `HOLD_MS`.
    Repeat(u32, u64, Cond),
}

/// Splits at commas outside parentheses.
fn split_steps(script: &str) -> Vec<&str> {
    let (mut steps, mut depth, mut from) = (Vec::new(), 0i32, 0);
    for (i, c) in script.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                steps.push(&script[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    steps.push(&script[from..]);
    steps
}

/// `VICTORY_AUTO_INPUT` as steps: a button name (tapped for `TAP_MS`), a wait
/// `w<seconds>`, `@<seconds>` (until that long after the process start, a
/// no-op once past), a wait on the screen `d>=N` / `d<N` (the most recent
/// presented frame's draw count satisfies it for 500 ms), or
/// `rep(<button>,<seconds>,<cond>)` (the button tapped every period until
/// the condition holds). Buttons and waits take an optional repeat `x<n>`
/// (`Downx3`, `w1x2`). Steps that do not parse are skipped and returned.
fn parse_script(script: &str) -> (Vec<Step>, Vec<String>) {
    let mut steps = Vec::new();
    let mut bad = Vec::new();
    for step in split_steps(script).into_iter().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(seconds) = step.strip_prefix('@') {
            match millis(seconds) {
                Some(ms) => steps.push(Step::At(ms)),
                None => bad.push(step.to_owned()),
            }
            continue;
        }
        if let Some(cond) = Cond::parse(step) {
            steps.push(Step::Until(cond));
            continue;
        }
        if let Some(inner) = step.strip_prefix("rep(").and_then(|s| s.strip_suffix(')')) {
            let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
            match parts.as_slice() {
                [button, seconds, cond] => match (button_named(button), millis(seconds), Cond::parse(cond)) {
                    (Some(b), Some(ms), Some(c)) if ms > 0 => steps.push(Step::Repeat(b, ms, c)),
                    _ => bad.push(step.to_owned()),
                },
                _ => bad.push(step.to_owned()),
            }
            continue;
        }
        let (body, times) = match step.rsplit_once(['x', 'X']) {
            Some((body, n)) if !body.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                match n.parse::<u32>() {
                    Ok(n) => (body, n),
                    Err(_) => {
                        bad.push(step.to_owned());
                        continue;
                    }
                }
            }
            _ => (step, 1),
        };
        let wait = body.strip_prefix(['w', 'W']).and_then(millis);
        match (wait, button_named(body)) {
            (Some(ms), _) => steps.extend((0..times).map(|_| Step::Wait(ms))),
            (None, Some(buttons)) => steps.extend((0..times).map(|_| Step::Tap(buttons))),
            (None, None) => bad.push(step.to_owned()),
        }
    }
    (steps, bad)
}

/// Where the script is within its current step.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Fresh,
    /// A tap held over `from..from + TAP_MS`.
    Tapping { from: u64 },
    /// A wait on the screen: begun at `started`, the condition holding since `since`.
    Waiting { started: u64, since: Option<u64> },
    /// A repeat: begun at `started`, the next tap due at `next`, the last held
    /// until `until`, the condition holding since `since`.
    Repeating { started: u64, next: u64, until: u64, since: Option<u64> },
}

/// A `VICTORY_AUTO_INPUT` script run against the clock and the screen:
/// advanced at each input poll with the milliseconds since the start and
/// the draw count of the most recent presented frame, it says which
/// buttons are held (the same call twice gives the same answer). Taps not
/// placed by an `@` or a wait on the screen come `gap` ms after the tap
/// before them; timed steps count from where the step before ended, as
/// planned, not from when the poll came.
struct Script {
    steps: Vec<Step>,
    gap: u64,
    i: usize,
    /// When the current step begins.
    at: u64,
    /// A tap came last, with no `@` or screen wait since.
    after_tap: bool,
    phase: Phase,
    /// Given up: no more presses.
    stopped: bool,
    /// What the steps said (waits done, giving up), for the log: (warning, text).
    said: Vec<(bool, String)>,
    /// Taps begun: when and which buttons.
    tapped: Vec<(u64, u32)>,
}

impl Script {
    fn new(steps: Vec<Step>, start: u64, gap: u64) -> Script {
        Script {
            steps,
            gap,
            i: 0,
            at: start,
            after_tap: false,
            phase: Phase::Fresh,
            stopped: false,
            said: Vec::new(),
            tapped: Vec::new(),
        }
    }

    fn next(&mut self, at: u64) {
        self.i += 1;
        self.at = at;
        self.phase = Phase::Fresh;
    }

    /// The buttons held at `now` ms with `draws` on the screen.
    fn advance(&mut self, now: u64, draws: u32) -> u32 {
        while !self.stopped && self.i < self.steps.len() {
            match self.steps[self.i].clone() {
                Step::At(ms) => {
                    self.after_tap = false;
                    let at = self.at.max(ms);
                    self.next(at);
                }
                Step::Wait(ms) => {
                    let at = self.at + ms;
                    self.next(at);
                }
                Step::Tap(buttons) => {
                    let from = match self.phase {
                        Phase::Tapping { from } => from,
                        _ => {
                            let from = self.at + if self.after_tap { self.gap } else { 0 };
                            self.phase = Phase::Tapping { from };
                            self.tapped.push((from, buttons));
                            from
                        }
                    };
                    if now < from {
                        return 0;
                    }
                    if now < from + TAP_MS {
                        return buttons;
                    }
                    self.after_tap = true;
                    self.next(from + TAP_MS);
                }
                Step::Until(cond) => {
                    if now < self.at {
                        return 0;
                    }
                    let (started, since) = match self.phase {
                        Phase::Waiting { started, since } => (started, since),
                        _ => (now, None),
                    };
                    let since = cond.holds(draws).then(|| since.unwrap_or(now));
                    if since.is_some_and(|s| now - s >= HOLD_MS) {
                        self.said.push((false, format!("{} after {:.1} s", cond.text, (now - started) as f64 / 1e3)));
                        self.after_tap = false;
                        self.next(now);
                        continue;
                    }
                    if now - started >= GIVE_UP_MS {
                        self.give_up(&cond.text);
                        return 0;
                    }
                    self.phase = Phase::Waiting { started, since };
                    return 0;
                }
                Step::Repeat(buttons, period, cond) => {
                    if now < self.at {
                        return 0;
                    }
                    let (started, next, until, since) = match self.phase {
                        Phase::Repeating { started, next, until, since } => (started, next, until, since),
                        _ => (now, now, 0, None),
                    };
                    if now < until {
                        // A tap under way is held to its end.
                        return buttons;
                    }
                    // Looked at before each tap: none lands once the screen changed.
                    let since = cond.holds(draws).then(|| since.unwrap_or(now));
                    if since.is_some_and(|s| now - s >= HOLD_MS) {
                        self.said.push((
                            false,
                            format!("rep until {} after {:.1} s", cond.text, (now - started) as f64 / 1e3),
                        ));
                        self.after_tap = false;
                        self.next(now);
                        continue;
                    }
                    if now - started >= GIVE_UP_MS {
                        self.give_up(&cond.text);
                        return 0;
                    }
                    if since.is_none() && now >= next {
                        self.tapped.push((now, buttons));
                        self.phase = Phase::Repeating { started, next: now + period, until: now + TAP_MS, since };
                        return buttons;
                    }
                    self.phase = Phase::Repeating { started, next, until, since };
                    return 0;
                }
            }
        }
        0
    }

    fn give_up(&mut self, cond: &str) {
        self.stopped = true;
        self.said.push((true, format!("gave up waiting for {cond}")));
    }
}

/// `VICTORY_AUTO_INPUT`, `VICTORY_AUTO_INPUT_START_S` (default 0),
/// `VICTORY_AUTO_INPUT_GAP_MS` (default 400), parsed once.
static AUTO_INPUT: LazyLock<Option<Mutex<Script>>> = LazyLock::new(|| {
    let script = std::env::var("VICTORY_AUTO_INPUT").ok()?;
    let number = |name: &str, default: f64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(default)
    };
    let start_s = number("VICTORY_AUTO_INPUT_START_S", 0.0);
    let gap = number("VICTORY_AUTO_INPUT_GAP_MS", 400.0).round() as u64;
    let (steps, bad) = parse_script(&script);
    for step in bad {
        tracing::warn!("auto input: ignoring step {step:?}");
    }
    LazyLock::force(&START);
    tracing::info!("auto input: {} steps, starting at {start_s} s", steps.len());
    for (i, step) in steps.iter().enumerate() {
        tracing::debug!(step = i, ?step, "auto input step");
    }
    Some(Mutex::new(Script::new(steps, (start_s * 1000.0).round() as u64, gap)))
});

/// The scripted buttons held now; what the script has to say is logged.
fn scripted_now(elapsed_ms: u64) -> u32 {
    let Some(script) = AUTO_INPUT.as_ref() else {
        return 0;
    };
    let draws = victory_gpu::frame::PRESENTED_DRAWS.load(Ordering::Relaxed);
    let mut script = lock(script);
    let buttons = script.advance(elapsed_ms, draws);
    script.tapped.clear();
    for (warn, line) in std::mem::take(&mut script.said) {
        if warn {
            tracing::warn!("auto input: {line}");
        } else {
            tracing::info!("auto input: {line}");
        }
    }
    buttons
}

/// Pad buttons the auto inputs hold now (`VICTORY_AUTO_A`, `VICTORY_AUTO_INPUT`).
#[cfg_attr(not(windows), allow(dead_code))]
fn auto_buttons() -> u32 {
    let elapsed = START.elapsed();
    let a = if *AUTO_A > 0 && auto_a_pressed(elapsed, *AUTO_A) { A } else { 0 };
    a | scripted_now(elapsed.as_millis() as u64)
}

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            (pads, source.keys(), source.mouse())
        };
        let events = self
            .state()
            .poll(&self.devices, self.one_pad, pads, keys, mouse);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: Option<MouseState>,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        self.update(devices, MOUSE, mouse.map(Value::Mouse), now, &mut events);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(fallback(&keys), merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then(|| Value::Pad(fallback(&keys)));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, kind = kind(index), connected = !was, "input device");
            if index == MOUSE && !slot.connected {
                slot.history.clear();
            }
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; the title walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; the title registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub.returned.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    let hub = hub(this);
    match hub.devices.iter().find(|d| {
        d.info.id == *id && (d.index != MOUSE || hub.state().slots[MOUSE].connected)
    }) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, ..] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        _ => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
extern "system" fn mouse_state(this: *const Reading, out: *mut MouseState) -> bool {
    match &reading(this).value {
        Value::Mouse(state) if !out.is_null() => {
            // SAFETY: the caller supplies room for the v0 state.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    mouse_state as *const (),       // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, and keyboard and mouse while our process has focus.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        previous_cursor: Option<(i32, i32)>,
        mouse: MouseState,
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            #[allow(unused_mut)]
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            if slot == 0 {
                for vk in scripted() {
                    if let Some(&(_, button)) = super::FALLBACK_BUTTONS.iter().find(|(key, _)| *key == vk) {
                        pad.buttons |= button;
                    }
                }
            }
            // The auto press likewise reaches a real first pad, which hides
            // the stand-in.
            if slot == 0 {
                pad.buttons |= super::auto_buttons();
            }
            Some(pad)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            let mut down = scripted();
            if focused_window().is_some() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            // Outside the focus check: the keys feed the keyboard device and,
            // with no pad connected, the stand-in pad (`State::poll`).
            // Each button goes in as its stand-in key, which `fallback` maps
            // back to the same pad button (Down is the pad's D-pad Down).
            let auto = super::auto_buttons();
            for &(vk, button) in &super::FALLBACK_BUTTONS {
                if auto & button != 0 && !down.contains(&vk) {
                    down.push(vk);
                }
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn mouse(&mut self) -> Option<MouseState> {
            static ENABLED: LazyLock<bool> =
                LazyLock::new(|| std::env::var_os("VICTORY_GAMEPAD_MOUSE").is_none_or(|v| v != "0"));
            if !*ENABLED {
                return None;
            }
            let (mut point, mut rect) = (POINT::default(), RECT::default());
            let position = focused_window().and_then(|window| {
                // SAFETY: all outputs are local and all coordinates use the same window.
                let sampled = unsafe {
                    GetCursorPos(&mut point) != 0
                        && ScreenToClient(window, &mut point) != 0
                        && GetClientRect(window, &mut rect) != 0
                        && GetForegroundWindow() == window
                };
                sampled.then_some((point.x, point.y))
            });
            let (cursor, (dx, dy)) =
                mouse_delta(self.previous_cursor, position, (rect.right, rect.bottom));
            self.previous_cursor = cursor;
            if cursor.is_none() {
                self.mouse = MouseState::default();
                return None;
            }
            self.mouse.position_x += dx;
            self.mouse.position_y += dy;
            self.mouse.buttons = 0;
            for (bit, vk) in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON].into_iter().enumerate() {
                // SAFETY: virtual-key state query.
                if unsafe { GetAsyncKeyState(i32::from(vk)) } < 0 {
                    self.mouse.buttons |= 1 << bit;
                }
            }
            Some(self.mouse)
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons).
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window is one of ours (the title's or our fallback).
    fn focused_window() -> Option<HWND> {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            let window = GetForegroundWindow();
            GetWindowThreadProcessId(window, &mut process);
            (process == GetCurrentProcessId()).then_some(window)
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: Option<MouseState>,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn mouse(&mut self) -> Option<MouseState> {
            self.0.lock().unwrap().mouse
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))), one_pad);
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn mouse_motion_clamps_to_client_and_resets_on_focus_loss() {
        let mut previous = None;
        for (point, delta) in [
            (Some((50, 40)), (0, 0)),
            (Some((30, 10)), (-20, -30)),
            (Some((200, -5)), (69, -10)),
            (Some((201, -8)), (0, 0)),
            (None, (0, 0)),
            (Some((7, 9)), (0, 0)),
        ] {
            let (cursor, motion) = mouse_delta(previous, point, (100, 80));
            assert_eq!(motion, delta);
            previous = cursor;
        }
    }

    #[test]
    fn mouse_readings_and_callbacks_in_both_pad_modes() {
        for one_pad in [false, true] {
            let (hub, host) = fake_with(one_pad);
            let log = Log::default();
            register(hub, MOUSE_KIND, 2, &log);
            let sample = MouseState {
                buttons: 5,
                position_x: -3,
                ..Default::default()
            };
            host.lock().unwrap().mouse = Some(sample);
            hub.poll();
            let r = current(hub, MOUSE_KIND, MOUSE).unwrap();
            let get =
                slot::<extern "system" fn(*const Reading, *mut MouseState) -> bool, _>(r, 0x80);
            let mut got = MouseState::default();
            assert!(get(r, &mut got));
            assert_eq!(got, sample);
            release(r);

            host.lock().unwrap().mouse = None;
            hub.poll();
            assert_eq!(current(hub, MOUSE_KIND, MOUSE), Err(DEVICE_DISCONNECTED));
            assert!(hub.state().slots[MOUSE].history.is_empty());
            assert_eq!(*log.lock().unwrap(), [(MOUSE, 3, 0), (MOUSE, 0, 3)]);
        }
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            _ => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState { buttons: A, left_y: -0.5, ..Default::default() });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(&hub.object, std::ptr::null(), GAMEPAD_KIND, 1, 0, context, Some(slow), &mut token),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(!unregister(&hub.object, token, 5000), "times out while it runs");
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }

    #[test]
    fn auto_a_holds_a_quarter_second_in_every_four() {
        let at = |ms| auto_a_pressed(Duration::from_millis(ms), 10);
        assert!(at(0) && at(249) && at(4000) && at(8249));
        assert!(!at(250) && !at(3999) && !at(4250));
        // Off after the window, and when the switch is 0.
        assert!(!at(12_000) && !at(10_000));
        assert!(!auto_a_pressed(Duration::ZERO, 0));
    }

    /// Runs a script over `until` ms in 10 ms polls with `draws(ms)` on the
    /// screen: the taps begun (when, buttons), the buttons held at each poll,
    /// and what it said.
    fn run(script: &str, start: u64, gap: u64, until: u64, draws: impl Fn(u64) -> u32) -> (Script, Vec<u32>) {
        let (steps, bad) = parse_script(script);
        assert!(bad.is_empty(), "{bad:?}");
        let mut s = Script::new(steps, start, gap);
        let held = (0..=until / 10).map(|i| s.advance(i * 10, draws(i * 10))).collect();
        (s, held)
    }

    /// The taps of a timed script, without running it to the end.
    fn taps(script: &str, start: u64, gap: u64) -> Vec<(u64, u32)> {
        run(script, start, gap, 120_000, |_| 0).0.tapped
    }

    #[test]
    fn auto_input_script_taps_waits_and_repeats() {
        // Start 1 s, gap 400: A at 1000, wait 3 s, Down x3 from 4500.
        let (s, held) = run("A, w3, Downx3, w0.5, lb", 1000, 400, 100_000, |_| 0);
        let froms: Vec<u64> = s.tapped.iter().map(|t| t.0).collect();
        assert_eq!(froms, [1000, 4500, 5000, 5500, 6500]);
        let at = |ms: usize| held[ms / 10];
        assert_eq!((at(990), at(1000), at(1090), at(1100)), (0, A, A, 0));
        assert_eq!((at(4500), at(5050), at(5550), at(5600)), (DOWN, DOWN, DOWN, 0));
        assert_eq!(at(6550), LEFT_SHOULDER);
        assert_eq!(at(100_000), 0);
    }

    #[test]
    fn auto_input_skips_malformed_steps() {
        let (steps, bad) = parse_script("Jump,A,wx,w-1,Downx,,@x,@-2,Startx2,rep(A,0,d>1),rep(Q,1,d>1),d=3");
        assert_eq!(bad, ["Jump", "wx", "w-1", "Downx", "@x", "@-2", "rep(A,0,d>1)", "rep(Q,1,d>1)", "d=3"]);
        assert_eq!(steps, [Step::Tap(A), Step::Tap(MENU), Step::Tap(MENU)]);
        let t = taps("A,Startx2", 0, 0);
        assert_eq!(t.iter().map(|t| t.1).collect::<Vec<_>>(), [A, MENU, MENU]);
        assert_eq!(t[2].0, 2 * TAP_MS);
    }

    #[test]
    fn auto_input_replays_the_recorded_start() {
        let script = "@7.6,A,@8.9,A,@9.1,A,@9.3,A,@9.5,A,@24,A,@24.4,A,@24.9,A,@26.1,A,@27.3,A,@28.5,Down,@29.4,Down,@34,Down,@34.35,A,@35.65,A,@36.2,Down,@36.55,A,@36.9,Down,@37.35,A";
        // The start offset does not move `@` times; the gap does not apply.
        let (s, held) = run(script, 0, 400, 40_000, |_| 0);
        let expected = [
            (7600, A), (8900, A), (9100, A), (9300, A), (9500, A),
            (24000, A), (24400, A), (24900, A), (26100, A), (27300, A),
            (28500, DOWN), (29400, DOWN), (34000, DOWN), (34350, A), (35650, A),
            (36200, DOWN), (36550, A), (36900, DOWN), (37350, A),
        ];
        assert_eq!(s.tapped, expected);
        assert_eq!(held[34_400 / 10], A);
        assert_eq!(held[34_460 / 10], 0);
        // Aliases and an `@` already past.
        assert_eq!(taps("Enter,@0,ArrowDown", 0, 400), [(0, A), (100, DOWN)]);
    }

    #[test]
    fn auto_input_parses_the_screen_script() {
        let (steps, bad) = parse_script("@7,rep(A,1.5,d>=20),w0.8,Down,Down,d>=40,w0.8,Down,A,w1.2,A,Down,A,Down,A");
        assert!(bad.is_empty(), "{bad:?}");
        let ge = |n| Cond { text: format!("d>={n}"), op: Op::Ge, n };
        assert_eq!(
            steps,
            [
                Step::At(7000),
                Step::Repeat(A, 1500, ge(20)),
                Step::Wait(800),
                Step::Tap(DOWN),
                Step::Tap(DOWN),
                Step::Until(ge(40)),
                Step::Wait(800),
                Step::Tap(DOWN),
                Step::Tap(A),
                Step::Wait(1200),
                Step::Tap(A),
                Step::Tap(DOWN),
                Step::Tap(A),
                Step::Tap(DOWN),
                Step::Tap(A),
            ]
        );
        assert_eq!(parse_script("d<5").0, [Step::Until(Cond { text: "d<5".into(), op: Op::Lt, n: 5 })]);
    }

    #[test]
    fn a_screen_wait_holds_until_the_count_held_for_half_a_second() {
        // 40 draws from 2 s, a dip at 2.2 s, then 40 again from 2.3 s.
        let draws = |ms: u64| if (2000..2200).contains(&ms) || ms >= 2300 { 40 } else { 5 };
        let (s, held) = run("d>=40,A", 0, 400, 5000, draws);
        // Held since 2.3 s: the wait ends at 2.8 s, and A is tapped then.
        assert_eq!(s.tapped, [(2800, A)]);
        assert_eq!(held[2790 / 10], 0);
        assert_eq!(held[2800 / 10], A);
    }

    #[test]
    fn a_repeat_stops_tapping_once_the_screen_changed() {
        // The menu shows 25 draws from 4.05 s on.
        let draws = |ms: u64| if ms >= 4050 { 25 } else { 4 };
        let (s, _) = run("rep(A,1.5,d>=20),Down", 0, 400, 10_000, draws);
        // A at 0, 1.5, 3 s; none at 4.5 s (the screen changed at 4.05 s);
        // the condition held from then for 500 ms, so Down at 4.55 s.
        assert_eq!(s.tapped, [(0, A), (1500, A), (3000, A), (4550, DOWN)]);
    }

    #[test]
    fn a_wait_that_never_ends_stops_the_script() {
        let (mut s, _) = run("A,d>=40,B", 0, 400, GIVE_UP_MS + 1000, |_| 5);
        assert!(s.stopped);
        assert_eq!(s.tapped, [(0, A)]);
        assert_eq!(s.said, [(true, "gave up waiting for d>=40".to_owned())]);
        // No further presses.
        assert_eq!(s.advance(GIVE_UP_MS + 5000, 100), 0);
        assert_eq!(s.tapped.len(), 1);
    }
}
