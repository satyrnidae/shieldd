//! Turns controller reports into the desktop's input: a virtual Xbox 360 pad per controller
//! (Gamepad), plus the trackpads as the mouse and the volume keys as media keys, shared by all of
//! them (Desktop). Same mapping as the old WSL bridge (`shield_tx.py`). Games' rumble on a
//! virtual pad comes back out through `take_rumble`.
//!
//! What a pad shows and how the shared mouse and keys behave is here; how they reach the system
//! is per platform: ViGEmBus and SendInput on Windows (windows.rs), uinput on Linux (linux.rs).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use shdlink::xb;
use shdnet::mac::Mac;
use tracing::debug;

#[cfg(target_os = "linux")]
pub use linux::Gamepad;
#[cfg(target_os = "linux")]
use linux::Input;
#[cfg(target_os = "windows")]
pub use windows::Gamepad;
#[cfg(target_os = "windows")]
use windows::Input;

use crate::controller::{Pad, Trackpad, btn, key};

/// The system plays its own device sound when a virtual pad first plugs in: Windows does for
/// ViGEm's. A desktop on Linux has nothing to say about a uinput device.
pub const PLUG_SOUND: bool = cfg!(target_os = "windows");

/// What a virtual pad shows, shaped as XInput has it: XUSB button bits (shdlink's `xb`),
/// triggers 0-255, and sticks with Y growing upward
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

impl Gamepad {
    /// Neutral pad. For when input stops arriving.
    pub fn release(&mut self) {
        self.update(&PadState::default());
    }

    pub fn pad(&mut self, p: &Pad) {
        self.update(&to_pad_state(p));
    }
}

/// Set in `Rumble` while a value is waiting to be taken
const RUMBLE_NEW: u32 = 1 << 16;

/// The latest rumble a game set on a virtual pad, from whichever thread hears it, until
/// `take_rumble` takes it
#[derive(Debug, Clone, Default)]
struct Rumble(Arc<AtomicU32>);

impl Rumble {
    fn set(&self, large: u8, small: u8) {
        self.0.store(RUMBLE_NEW | u32::from(large) << 8 | u32::from(small), Ordering::Relaxed);
    }

    /// Large and small motor, if they changed since the last call
    fn take(&self) -> Option<(u8, u8)> {
        let v = self.0.swap(0, Ordering::Relaxed);
        (v & RUMBLE_NEW != 0).then_some(((v >> 8) as u8, v as u8))
    }

    fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

//region Desktop

/// The media keys a controller has
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKey {
    VolumeUp,
    VolumeDown,
    Mute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseButton {
    Left,
    Right,
    Middle,
    /// Back; only from shield_tx.py in receiver mode
    X1,
}

/// Controller key bit -> media key
const MEDIA_KEYS: [(u8, MediaKey); 3] =
    [(key::VOL_UP, MediaKey::VolumeUp), (key::VOL_DOWN, MediaKey::VolumeDown), (key::MUTE, MediaKey::Mute)];

/// Keys that repeat while held, like a keyboard's, where the platform doesn't do it for us. The
/// controller only reports the level. Mute toggles, so it doesn't.
const REPEATING: u8 = key::VOL_UP | key::VOL_DOWN;
/// Every key bit in MEDIA_KEYS
const MEDIA_BITS: u8 = key::VOL_UP | key::VOL_DOWN | key::MUTE;

/// Mouse button bit -> button. The trackpad only has the first three.
const MOUSE_BUTTONS: [(u8, MouseButton); 4] =
    [(0x01, MouseButton::Left), (0x02, MouseButton::Right), (0x04, MouseButton::Middle), (0x08, MouseButton::X1)];
/// Every bit in MOUSE_BUTTONS
const MOUSE_BITS: u8 = 0x0f;

/// Bits each source holds. The system sees them all OR'd together, so a button stays down while
/// any controller holds it.
#[derive(Debug, Default)]
struct Held {
    by: HashMap<Mac, u8>,
}

impl Held {
    fn all(&self) -> u8 {
        self.by.values().fold(0, |a, b| a | b)
    }

    /// Sets `src`'s bits; returns everyone's together before and after
    fn set(&mut self, src: Mac, bits: u8) -> (u8, u8) {
        let was = self.all();
        if bits == 0 {
            self.by.remove(&src);
        } else {
            self.by.insert(src, bits);
        }
        (was, self.all())
    }
}

/// Pointer acceleration: how much of the full scale a finger's speed gets
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accel {
    /// Gain at a standstill, as a fraction of full. 1.0 or more turns acceleration off.
    pub min: f32,
    /// Finger speed in counts per ms where the gain reaches full
    pub knee: f32,
}

impl Accel {
    /// Whether this eases anything at all. Written so NaN lands on the off side too: anything out
    /// of range is off, not a poisoned carry.
    pub fn enabled(&self) -> bool {
        (0.0..1.0).contains(&self.min) && self.knee > 0.0
    }

    /// Fraction of the full scale at `speed` counts per ms: `min` at rest, smoothstep up to 1.0 at
    /// the knee
    fn gain(&self, speed: f32) -> f32 {
        if !self.enabled() {
            return 1.0;
        }
        let t = (speed / self.knee).clamp(0.0, 1.0);
        self.min + (1.0 - self.min) * t * t * (3.0 - 2.0 * t)
    }
}

/// Trackball emulation: after a flick and a lift the pointer keeps gliding, and slows to a stop
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trackball {
    /// Time constant of the slowdown in ms: the glide has lost about two thirds of its speed
    /// after this long
    pub glide_ms: f32,
}

/// How far back from the lift to read the finger's speed, in ms
const WINDOW_MS: f32 = 80.0;
/// Least time the recent reports must span to call a speed, in ms
const FLING_MIN_SPAN_MS: f32 = 4.0;
/// Slowest release, in pixels per ms, that still flings. Fine positioning stays under this.
const FLING_MIN_PX_MS: f32 = 1.0;
/// A glide slower than this, in pixels per ms, is over
const GLIDE_END_PX_MS: f32 = 0.05;

/// A pointer coasting after a flick
#[derive(Debug, Clone, Copy)]
struct Glide {
    /// Pixels per ms
    v: (f32, f32),
    /// Sub-pixel motion carried between ticks
    carry: (f32, f32),
    last: Instant,
}

impl Glide {
    /// Whole pixels to move by `now`; the speed decays exponentially with `glide_ms` as its
    /// time constant. The distance over the tick is the exact integral, so a late tick catches
    /// up without overshooting.
    fn step(&mut self, now: Instant, glide_ms: f32) -> (i32, i32) {
        let ms = now.saturating_duration_since(self.last).as_secs_f32() * 1000.0;
        self.last = now;
        if !(glide_ms > 0.0) {
            self.v = (0.0, 0.0);
            return (0, 0);
        }
        let left = (-ms / glide_ms).exp();
        let reach = glide_ms * (1.0 - left);
        let x = self.v.0 * reach + self.carry.0;
        let y = self.v.1 * reach + self.carry.1;
        let (ix, iy) = (x.trunc(), y.trunc());
        self.carry = (x - ix, y - iy);
        self.v = (self.v.0 * left, self.v.1 * left);
        (ix as i32, iy as i32)
    }

    fn done(&self) -> bool {
        self.v.0.hypot(self.v.1) < GLIDE_END_PX_MS
    }
}

/// A source's finger on its trackpad
#[derive(Debug, Default)]
struct Pointer {
    /// What the last WINDOW_MS of reports moved, in pixels before rounding, for the speed at a lift
    recent: VecDeque<(Instant, f32, f32)>,
    /// Coasting after a flick, with the finger up
    glide: Option<Glide>,
    /// Last finger position while touching
    touch: Option<(u16, u16)>,
    /// Sub-pixel motion carried between reports
    carry: (f32, f32),
    /// Smoothed finger speed in counts per ms
    speed: f32,
    /// When the last move report came in
    last: Option<Instant>,
}

/// How far each report pulls the smoothed speed toward its own, 0 to 1
const SPEED_SMOOTHING: f32 = 0.5;
/// Floor on the time between reports when working out speed, in ms, so a burst doesn't read as
/// infinitely fast
const MIN_REPORT_MS: f32 = 1.0;
/// A gap this long, in ms, means the finger lifted and came back, where the caller has no lift
/// to tell us: the old speed is dropped and the report goes at the slow end, like a first one
const PAUSE_MS: f32 = 50.0;

impl Pointer {
    /// The glide a lift at `now` leaves behind, if the finger was moving fast enough when it left.
    /// Speed is what the recent reports moved over the time they span, so a finger that slowed or
    /// rested before lifting reads slow.
    fn lift(&self, now: Instant) -> Option<Glide> {
        let &(first, ..) = self.recent.front()?;
        let &(last, ..) = self.recent.back()?;
        let span = last.duration_since(first).as_secs_f32() * 1000.0;
        let quiet = now.saturating_duration_since(last).as_secs_f32() * 1000.0;
        if span < FLING_MIN_SPAN_MS || quiet > PAUSE_MS {
            return None;
        }
        // The oldest report moved before the span starts
        let (sx, sy) = self.recent.iter().skip(1).fold((0.0, 0.0), |a, &(_, x, y)| (a.0 + x, a.1 + y));
        let v = (sx / span, sy / span);
        (v.0.hypot(v.1) >= FLING_MIN_PX_MS).then_some(Glide { v, carry: (0.0, 0.0), last: now })
    }

    /// The finger left the pad at `now`: starts over, and with `trackball` on, leaves a glide if
    /// it was flicked. A report that finds the finger already up keeps the glide going.
    fn lifted(&mut self, now: Instant, trackball: bool) {
        let glide = match (trackball, self.touch.is_some()) {
            (false, _) => None,
            (true, true) => self.lift(now),
            (true, false) => self.glide.take(),
        };
        *self = Pointer { glide, ..Pointer::default() };
    }

    /// The finger came down: catches any glide
    fn touched(&mut self) {
        self.glide = None;
    }

    /// Whole pixels to move for a report of `dx` and `dy` counts at `now`; the rest is carried
    fn step(&mut self, dx: f32, dy: f32, scale: (f32, f32), accel: Accel, now: Instant) -> (i32, i32) {
        // The first report of a touch has no gap to measure, so it goes at the slow end
        if let Some(last) = self.last.replace(now) {
            let ms = now.saturating_duration_since(last).as_secs_f32() * 1000.0;
            if ms > PAUSE_MS {
                self.speed = 0.0;
                self.recent.clear();
            } else {
                self.speed += SPEED_SMOOTHING * (dx.hypot(dy) / ms.max(MIN_REPORT_MS) - self.speed);
            }
        }
        let gain = accel.gain(self.speed);
        let (mx, my) = (dx * scale.0 * gain, dy * scale.1 * gain);
        self.recent.push_back((now, mx, my));
        while self.recent.front().is_some_and(|&(t, ..)| now.saturating_duration_since(t).as_secs_f32() * 1000.0 > WINDOW_MS) {
            self.recent.pop_front();
        }
        let x = mx + self.carry.0;
        let y = my + self.carry.1;
        let (ix, iy) = (x.trunc(), y.trunc());
        self.carry = (x - ix, y - iy);
        (ix as i32, iy as i32)
    }
}

/// The mouse and media keys, shared by every controller: trackpads move one pointer, and a
/// key or button goes down with the first source to press it and up with the last to let go.
pub struct Desktop {
    input: Input,
    keys: Held,
    buttons: Held,
    pointers: HashMap<Mac, Pointer>,
    /// Next repeat while a REPEATING key is held, and the interval after it
    next_repeat: Option<(Instant, Duration)>,
    /// Trackpad counts -> pixels at full gain, x and y
    mouse_scale: (f32, f32),
    /// How finger speed eases that scale down
    accel: Accel,
    /// Glide after a flick, if on
    trackball: Option<Trackball>,
}

impl Desktop {
    pub fn new(mouse_scale: (f32, f32), accel: Accel, trackball: Option<Trackball>) -> Self {
        Desktop {
            input: Input::new(),
            keys: Held::default(),
            buttons: Held::default(),
            pointers: HashMap::new(),
            next_repeat: None,
            mouse_scale,
            accel,
            trackball,
        }
    }

    /// Lets go of whatever `src` alone holds, and forgets its finger. For when its input stops.
    pub fn release(&mut self, src: &Mac) {
        self.set_keys(src, 0);
        self.set_mouse_buttons(src, 0);
        self.pointers.remove(src);
    }

    /// Relative move in trackpad counts, scaled to pixels and eased by finger speed
    pub fn move_counts(&mut self, src: &Mac, dx: f32, dy: f32) {
        let p = self.pointers.entry(*src).or_default();
        let (ix, iy) = p.step(dx, dy, self.mouse_scale, self.accel, Instant::now());
        if ix != 0 || iy != 0 {
            self.input.move_by(ix, iy);
        }
    }

    pub fn trackpad(&mut self, src: &Mac, t: &Trackpad) {
        let trackball = self.trackball.is_some();
        let p = self.pointers.entry(*src).or_default();
        if t.touch {
            p.touched();
            let last = p.touch.replace((t.x, t.y));
            if let Some((px, py)) = last {
                let dx = f32::from(t.x) - f32::from(px);
                let dy = f32::from(t.y) - f32::from(py);
                self.move_counts(src, dx, dy);
            }
        } else {
            p.lifted(Instant::now(), trackball);
        }
        self.set_mouse_buttons(src, t.buttons);
    }

    /// Controller key bits; media keys go out on each edge of everyone's together
    pub fn set_keys(&mut self, src: &Mac, keys: u8) {
        let (was, keys) = self.keys.set(*src, keys & MEDIA_BITS);
        let changed = was ^ keys;
        if changed == 0 {
            return;
        }
        debug!(keys = format!("{keys:#04x}"), was = format!("{was:#04x}"), "media keys");
        let edges: Vec<_> =
            MEDIA_KEYS.iter().filter(|(bit, _)| changed & bit != 0).map(|&(bit, k)| (k, keys & bit != 0)).collect();
        self.input.keys(&edges);
        if keys & REPEATING == 0 {
            self.next_repeat = None;
        } else if changed & keys & REPEATING != 0 {
            // Read per press, so a change in the keyboard settings applies right away
            self.next_repeat = Input::repeat().map(|(delay, interval)| (Instant::now() + delay, interval));
        }
    }

    /// Repeats held volume keys where the platform doesn't; call often, at least every repeat
    /// interval
    pub fn poll(&mut self, now: Instant) {
        self.glide(now);
        let Some((at, interval)) = self.next_repeat else { return };
        if now < at {
            return;
        }
        let keys = self.keys.all();
        let downs: Vec<_> =
            MEDIA_KEYS.iter().filter(|(bit, _)| keys & REPEATING & bit != 0).map(|&(_, k)| (k, true)).collect();
        let late_ms = now.duration_since(at).as_millis() as u64;
        debug!(keys = format!("{keys:#04x}"), late_ms, "media key repeat");
        self.input.keys(&downs);
        // A stalled caller gets one repeat, not a burst
        self.next_repeat = Some(((at + interval).max(now), interval));
    }

    /// Moves every pointer that's coasting after a flick
    fn glide(&mut self, now: Instant) {
        let Some(tb) = self.trackball else { return };
        for p in self.pointers.values_mut() {
            let Some(g) = p.glide.as_mut() else { continue };
            let (ix, iy) = g.step(now, tb.glide_ms);
            if g.done() {
                p.glide = None;
            }
            if ix != 0 || iy != 0 {
                self.input.move_by(ix, iy);
            }
        }
    }

    /// Mouse button bits as in MOUSE_BUTTONS; events go out on each edge of everyone's together
    pub fn set_mouse_buttons(&mut self, src: &Mac, buttons: u8) {
        let (was, buttons) = self.buttons.set(*src, buttons & MOUSE_BITS);
        let changed = was ^ buttons;
        if changed == 0 {
            return;
        }
        // The pointer is shared, so a click from anyone stops every glide before it lands. Only a
        // press does; a release can arrive with the lift that starts a glide.
        if buttons & changed != 0 {
            for p in self.pointers.values_mut() {
                p.touched();
            }
        }
        let edges: Vec<_> = MOUSE_BUTTONS
            .iter()
            .filter(|(bit, _)| changed & bit != 0)
            .map(|&(bit, b)| (b, buttons & bit != 0))
            .collect();
        self.input.buttons(&edges);
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // Never leave a key or mouse button held
        let sources: Vec<Mac> = self.keys.by.keys().chain(self.buttons.by.keys()).copied().collect();
        for src in sources {
            self.release(&src);
        }
    }
}

//endregion

//region Mapping

/// Controller pad -> what the virtual pad shows. Pure, so it can be tested.
pub fn to_pad_state(p: &Pad) -> PadState {
    let mut buttons = 0u16;
    for (from, to) in [
        (btn::A, xb::A),
        (btn::B, xb::B),
        (btn::X, xb::X),
        (btn::Y, xb::Y),
        (btn::LB, xb::LB),
        (btn::RB, xb::RB),
        (btn::L3, xb::LTHUMB),
        (btn::R3, xb::RTHUMB),
        (btn::START, xb::START),
    ] {
        if p.buttons & from != 0 {
            buttons |= to;
        }
    }
    if p.keys & key::BACK != 0 {
        buttons |= xb::BACK;
    }
    // Home is Guide. The NVIDIA button stays the controller's own, for connecting and power.
    if p.keys & key::HOME != 0 {
        buttons |= xb::GUIDE;
    }
    buttons |= match p.hat {
        0 => xb::UP,
        1 => xb::UP | xb::RIGHT,
        2 => xb::RIGHT,
        3 => xb::DOWN | xb::RIGHT,
        4 => xb::DOWN,
        5 => xb::DOWN | xb::LEFT,
        6 => xb::LEFT,
        7 => xb::UP | xb::LEFT,
        _ => 0,
    };
    PadState {
        buttons,
        lt: (p.lt >> 8) as u8,
        rt: (p.rt >> 8) as u8,
        lx: axis(p.lx, false),
        // HID Y grows downward, XInput Y grows upward
        ly: axis(p.ly, true),
        rx: axis(p.rx, false),
        ry: axis(p.ry, true),
    }
}

fn axis(v: u16, invert: bool) -> i16 {
    let c = i32::from(v) - 0x8000;
    let c = if invert { -c - 1 } else { c };
    c.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_until_the_last_one_lets_go() {
        let (a, b) = ([1; 6], [2; 6]);
        let mut h = Held::default();
        assert_eq!(h.set(a, 0x01), (0, 0x01));
        assert_eq!(h.set(b, 0x01), (0x01, 0x01), "already down");
        assert_eq!(h.set(b, 0), (0x01, 0x01), "a still holds it");
        assert_eq!(h.set(b, 0x02), (0x01, 0x03));
        assert_eq!(h.set(a, 0), (0x03, 0x02));
        assert_eq!(h.set(b, 0), (0x02, 0));
        assert!(h.by.is_empty());
    }

    #[test]
    fn rumble_is_taken_once() {
        let r = Rumble::default();
        assert_eq!(r.take(), None);
        r.set(0xFF, 0x10);
        assert_eq!(r.take(), Some((0xFF, 0x10)));
        assert_eq!(r.take(), None);
        // Both motors off is news too
        r.set(0, 0);
        assert_eq!(r.take(), Some((0, 0)));
    }

    #[test]
    fn neutral_pad_is_neutral() {
        let g = to_pad_state(&Pad::default());
        assert_eq!(g.buttons, 0);
        assert_eq!((g.lt, g.rt), (0, 0));
        // 0x7FFF is one count below center
        assert!(g.lx.abs() <= 1 && g.ly.abs() <= 1);
    }

    #[test]
    fn buttons_keys_and_hat() {
        let p = Pad { buttons: btn::A | btn::Y | btn::START, keys: key::HOME | key::BACK, hat: 3, ..Pad::default() };
        let g = to_pad_state(&p);
        assert_eq!(g.buttons, xb::A | xb::Y | xb::START | xb::GUIDE | xb::BACK | xb::DOWN | xb::RIGHT);
    }

    #[test]
    fn nvidia_button_maps_to_nothing() {
        let g = to_pad_state(&Pad { keys: key::NVIDIA, ..Pad::default() });
        assert_eq!(g.buttons, 0);
    }

    #[test]
    fn spurious_button_is_dropped() {
        let p = Pad { buttons: btn::SPURIOUS, ..Pad::default() };
        assert_eq!(to_pad_state(&p).buttons, 0);
    }

    const ACCEL: Accel = Accel { min: 0.35, knee: 1.0 };

    #[test]
    fn gain_is_min_when_still() {
        assert_eq!(ACCEL.gain(0.0), 0.35);
    }

    #[test]
    fn gain_is_full_at_the_knee_and_past_it() {
        assert_eq!(ACCEL.gain(1.0), 1.0);
        assert_eq!(ACCEL.gain(50.0), 1.0);
    }

    #[test]
    fn gain_rises_smoothly_between() {
        // Smoothstep is halfway at the middle of its range
        assert!((ACCEL.gain(0.5) - 0.675).abs() < 1e-6);
        let mut last = ACCEL.gain(0.0);
        for i in 1..=20 {
            let g = ACCEL.gain(i as f32 / 20.0);
            assert!(g >= last, "gain fell at step {i}");
            last = g;
        }
    }

    #[test]
    fn gain_off_is_always_full() {
        assert_eq!(Accel { min: 1.0, knee: 1.0 }.gain(0.0), 1.0);
        assert_eq!(Accel { min: 1.5, knee: 1.0 }.gain(0.0), 1.0, "past 1.0 is still off");
        assert_eq!(Accel { min: 0.35, knee: 0.0 }.gain(0.0), 1.0, "no knee, no ramp");
    }

    #[test]
    fn gain_out_of_range_is_off_not_garbage() {
        for a in [
            Accel { min: f32::NAN, knee: 1.0 },
            Accel { min: -0.5, knee: 1.0 },
            Accel { min: 0.35, knee: f32::NAN },
            Accel { min: 0.35, knee: -1.0 },
        ] {
            assert_eq!(a.gain(0.0), 1.0, "{a:?}");
            assert_eq!(a.gain(0.5), 1.0, "{a:?}");
        }
    }

    #[test]
    fn a_pause_starts_a_new_stroke() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 10, (10.0, 0.0), 8, ACCEL, Instant::now());
        // Finger lifted and came back with no reset in between, as in --receiver mode
        let (x, _) = p.step(1.0, 0.0, SCALE, ACCEL, now + Duration::from_secs(1));
        assert_eq!(x, 1, "old speed leaked into the new touch");
    }

    #[test]
    fn accel_enabled_only_in_range() {
        assert!(ACCEL.enabled());
        assert!(!OFF.enabled());
        assert!(!Accel { min: f32::NAN, knee: 1.0 }.enabled());
        assert!(!Accel { min: 0.35, knee: 0.0 }.enabled());
        assert!(!Accel { min: 0.35, knee: f32::NAN }.enabled());
    }

    const GLIDE_MS: f32 = 350.0;

    #[test]
    fn a_flick_leaves_a_glide_at_its_speed() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 12, (10.0, 0.0), 8, ACCEL, Instant::now());
        let g = p.lift(now + Duration::from_millis(8)).expect("a flick glides");
        // 30 px a report every 8 ms is 3.75 px/ms at full gain
        assert!(g.v.0 > 3.0 && g.v.0 <= 3.76, "{:?}", g.v);
        assert_eq!(g.v.1, 0.0);
    }

    #[test]
    fn flick_direction_is_kept() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 12, (-10.0, 5.0), 8, ACCEL, Instant::now());
        let g = p.lift(now + Duration::from_millis(8)).expect("a flick glides");
        assert!(g.v.0 < 0.0 && g.v.1 > 0.0, "{:?}", g.v);
    }

    #[test]
    fn a_slow_lift_glides_nothing() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 100, (1.0, 0.0), 8, ACCEL, Instant::now());
        assert!(p.lift(now + Duration::from_millis(8)).is_none());
    }

    #[test]
    fn resting_before_the_lift_glides_nothing() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 10, (10.0, 0.0), 8, ACCEL, Instant::now());
        // Finger stops but stays down for longer than the window, still reporting
        let (_, now) = feed(&mut p, 12, (0.0, 0.0), 8, ACCEL, now);
        assert!(p.lift(now + Duration::from_millis(8)).is_none());
    }

    #[test]
    fn a_lift_long_after_the_last_report_glides_nothing() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 12, (10.0, 0.0), 8, ACCEL, Instant::now());
        assert!(p.lift(now + Duration::from_millis(200)).is_none());
    }

    #[test]
    fn a_lone_report_has_no_speed_to_fling_with() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 1, (100.0, 0.0), 8, ACCEL, Instant::now());
        assert!(p.lift(now + Duration::from_millis(8)).is_none());
    }

    /// A pointer with the finger down and a flick behind it
    fn flicked() -> (Pointer, Instant) {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 12, (10.0, 0.0), 8, ACCEL, Instant::now());
        p.touch = Some((0, 0));
        (p, now + Duration::from_millis(8))
    }

    #[test]
    fn lifting_after_a_flick_starts_a_glide() {
        let (mut p, now) = flicked();
        p.lifted(now, true);
        assert!(p.glide.is_some());
        assert!(p.touch.is_none() && p.recent.is_empty(), "the stroke starts over");
    }

    #[test]
    fn trackball_off_never_glides() {
        let (mut p, now) = flicked();
        p.lifted(now, false);
        assert!(p.glide.is_none());
    }

    #[test]
    fn a_slow_lift_leaves_no_glide() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 100, (1.0, 0.0), 8, ACCEL, Instant::now());
        p.touch = Some((0, 0));
        p.lifted(now + Duration::from_millis(8), true);
        assert!(p.glide.is_none());
    }

    #[test]
    fn touching_down_catches_the_glide() {
        let (mut p, now) = flicked();
        p.lifted(now, true);
        p.touched();
        assert!(p.glide.is_none());
    }

    #[test]
    fn more_reports_with_the_finger_up_dont_end_the_glide() {
        let (mut p, now) = flicked();
        p.lifted(now, true);
        // A button-only report, say: touch is still false
        p.lifted(now + Duration::from_millis(8), true);
        assert!(p.glide.is_some());
    }

    /// Runs a glide with a tick every `ms` until it ends; returns the pixels it covered
    fn coast(mut g: Glide, ms: u64) -> i32 {
        let mut now = g.last;
        let mut total = 0;
        for _ in 0..100_000 {
            if g.done() {
                return total;
            }
            now += Duration::from_millis(ms);
            total += g.step(now, GLIDE_MS).0;
        }
        panic!("never stopped");
    }

    fn glide(vx: f32) -> Glide {
        Glide { v: (vx, 0.0), carry: (0.0, 0.0), last: Instant::now() }
    }

    #[test]
    fn a_glide_decays_to_a_stop_at_the_right_distance() {
        // Speed times the time constant is the distance an exponential slowdown covers
        let total = coast(glide(3.0), 16);
        let want = 3.0 * GLIDE_MS;
        assert!((total as f32 - want).abs() < 0.05 * want, "{total} vs {want}");
    }

    #[test]
    fn the_distance_doesnt_depend_on_the_tick_rate() {
        let (a, b) = (coast(glide(3.0), 4), coast(glide(3.0), 100));
        assert!((a - b).abs() <= 60, "{a} vs {b}");
    }

    #[test]
    fn a_stalled_tick_doesnt_overshoot() {
        let mut g = glide(3.0);
        let now = g.last + Duration::from_secs(10);
        let (x, _) = g.step(now, GLIDE_MS);
        assert!(x as f32 <= 3.0 * GLIDE_MS + 1.0, "{x}");
    }

    #[test]
    fn a_glide_moves_a_little_each_tick() {
        let mut g = glide(3.0);
        let (x, _) = g.step(g.last + Duration::from_millis(16), GLIDE_MS);
        // About 3 px/ms for 16 ms, a touch less for the slowdown
        assert!((40..=48).contains(&x), "{x}");
    }

    const SCALE: (f32, f32) = (3.0, 4.5);
    const OFF: Accel = Accel { min: 1.0, knee: 1.0 };

    /// Feeds `n` identical reports `ms` apart; returns each step and the clock after
    fn feed(p: &mut Pointer, n: usize, d: (f32, f32), ms: u64, accel: Accel, from: Instant) -> (Vec<(i32, i32)>, Instant) {
        let mut now = from;
        let mut steps = Vec::new();
        for _ in 0..n {
            now += Duration::from_millis(ms);
            steps.push(p.step(d.0, d.1, SCALE, accel, now));
        }
        (steps, now)
    }

    #[test]
    fn slow_motion_moves_in_small_steps() {
        let mut p = Pointer::default();
        let (steps, _) = feed(&mut p, 100, (1.0, 0.0), 8, ACCEL, Instant::now());
        assert!(steps.iter().all(|&(x, _)| x <= 2), "a single count skipped {:?}", steps.iter().max());
        // Flat scale would be 300; the slow end of the curve is around a third of that
        let total: i32 = steps.iter().map(|&(x, _)| x).sum();
        assert!((105..=135).contains(&total), "total {total}");
    }

    #[test]
    fn fast_motion_gets_the_full_scale() {
        let mut p = Pointer::default();
        let (steps, _) = feed(&mut p, 10, (10.0, 0.0), 8, ACCEL, Instant::now());
        assert_eq!(steps.last(), Some(&(30, 0)));
    }

    #[test]
    fn off_is_the_flat_scale() {
        let mut p = Pointer::default();
        let (steps, _) = feed(&mut p, 3, (1.0, 1.0), 8, OFF, Instant::now());
        assert!(steps.iter().all(|&(x, _)| x == 3), "{steps:?}");
        // 4.5 a report: the half carries over, so 4, 5, 4
        assert_eq!(steps.iter().map(|&(_, y)| y).collect::<Vec<_>>(), [4, 5, 4]);
    }

    #[test]
    fn a_fresh_finger_starts_slow() {
        let mut p = Pointer::default();
        let (steps, _) = feed(&mut p, 1, (10.0, 0.0), 8, ACCEL, Instant::now());
        // 10 counts at the 0.35 floor, not the 30 pixels of full gain
        assert_eq!(steps, [(10, 0)]);
    }

    #[test]
    fn reports_in_a_burst_dont_overshoot() {
        let mut p = Pointer::default();
        let t = Instant::now();
        p.step(10.0, 0.0, SCALE, ACCEL, t);
        // Same instant: the floor on the gap, so gain tops out at full instead of going wild
        let (x, _) = p.step(10.0, 0.0, SCALE, ACCEL, t);
        assert!(x <= 30, "{x}");
    }

    #[test]
    fn a_long_pause_doesnt_stall_or_spike() {
        let mut p = Pointer::default();
        let (_, now) = feed(&mut p, 10, (10.0, 0.0), 8, ACCEL, Instant::now());
        let (x, _) = p.step(10.0, 0.0, SCALE, ACCEL, now + Duration::from_secs(10));
        assert!((1..=30).contains(&x), "{x}");
    }

    #[test]
    fn axes_and_triggers() {
        let p = Pad { lx: 0, ly: 0, rx: 0xFFFF, ry: 0xFFFF, lt: 0xFFFF, rt: 0x8000, ..Pad::default() };
        let g = to_pad_state(&p);
        assert_eq!(g.lx, i16::MIN);
        assert_eq!(g.ly, i16::MAX, "stick pushed up = HID 0 = XInput max");
        assert_eq!(g.rx, i16::MAX);
        assert_eq!(g.ry, i16::MIN);
        assert_eq!((g.lt, g.rt), (0xFF, 0x80));
    }
}
