//! Turns controller reports into the desktop's input: a virtual Xbox 360 pad per controller
//! (Gamepad), plus the trackpads as the mouse and the volume keys as media keys, shared by all of
//! them (Desktop). Same mapping as the old WSL bridge (`shield_tx.py`). Games' rumble on a
//! virtual pad comes back out through `take_rumble`.
//!
//! What a pad shows and how the shared mouse and keys behave is here; how they reach the system
//! is per platform: ViGEmBus and SendInput on Windows (windows.rs), uinput on Linux (linux.rs).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use std::collections::HashMap;
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
#[cfg(windows)]
pub use windows::Gamepad;
#[cfg(windows)]
use windows::Input;

use crate::controller::{Pad, Trackpad, btn, key};

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

/// A source's finger on its trackpad
#[derive(Debug, Default)]
struct Pointer {
    /// Last finger position while touching
    touch: Option<(u16, u16)>,
    /// Sub-pixel motion carried between reports
    carry: (f32, f32),
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
    /// Trackpad counts -> pixels, x and y
    mouse_scale: (f32, f32),
}

impl Desktop {
    pub fn new(mouse_scale: (f32, f32)) -> Self {
        Desktop {
            input: Input::new(),
            keys: Held::default(),
            buttons: Held::default(),
            pointers: HashMap::new(),
            next_repeat: None,
            mouse_scale,
        }
    }

    /// Lets go of whatever `src` alone holds, and forgets its finger. For when its input stops.
    pub fn release(&mut self, src: &Mac) {
        self.set_keys(src, 0);
        self.set_mouse_buttons(src, 0);
        self.pointers.remove(src);
    }

    /// Relative move in trackpad counts, scaled to pixels
    pub fn move_counts(&mut self, src: &Mac, dx: f32, dy: f32) {
        let (sx, sy) = self.mouse_scale;
        let p = self.pointers.entry(*src).or_default();
        let x = dx * sx + p.carry.0;
        let y = dy * sy + p.carry.1;
        let (ix, iy) = (x.trunc(), y.trunc());
        p.carry = (x - ix, y - iy);
        if ix != 0.0 || iy != 0.0 {
            self.input.move_by(ix as i32, iy as i32);
        }
    }

    pub fn trackpad(&mut self, src: &Mac, t: &Trackpad) {
        let p = self.pointers.entry(*src).or_default();
        if t.touch {
            let last = p.touch.replace((t.x, t.y));
            if let Some((px, py)) = last {
                let dx = f32::from(t.x) - f32::from(px);
                let dy = f32::from(t.y) - f32::from(py);
                self.move_counts(src, dx, dy);
            }
        } else {
            *p = Pointer::default();
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

    /// Mouse button bits as in MOUSE_BUTTONS; events go out on each edge of everyone's together
    pub fn set_mouse_buttons(&mut self, src: &Mac, buttons: u8) {
        let (was, buttons) = self.buttons.set(*src, buttons & MOUSE_BITS);
        let changed = was ^ buttons;
        if changed == 0 {
            return;
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
