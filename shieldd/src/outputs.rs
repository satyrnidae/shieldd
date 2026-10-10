//! Turns controller reports into Windows input: a virtual Xbox 360 pad per controller through
//! ViGEmBus (Gamepad), plus the trackpads as the mouse and the volume keys as media keys
//! through SendInput, shared by all of them (Desktop). Same mapping as the old WSL bridge
//! (`shield_tx.py`). Games' rumble on a virtual pad comes back out through `take_rumble`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shdnet::mac::Mac;
use tracing::{debug, info, warn};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
    VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

use crate::controller::{Pad, Trackpad, btn, key};

/// Controller key bit -> Windows media key
const MEDIA_KEYS: [(u8, VIRTUAL_KEY); 3] =
    [(key::VOL_UP, VK_VOLUME_UP), (key::VOL_DOWN, VK_VOLUME_DOWN), (key::MUTE, VK_VOLUME_MUTE)];

/// Keys that repeat while held, like a keyboard's. The controller only reports the level, and
/// Windows doesn't repeat injected keys. Mute toggles, so it doesn't.
const REPEATING: u8 = key::VOL_UP | key::VOL_DOWN;
/// Every key bit in MEDIA_KEYS
const MEDIA_BITS: u8 = key::VOL_UP | key::VOL_DOWN | key::MUTE;

const XBUTTON1: u32 = 0x0001;

/// Mouse button bit -> (down, up, mouseData). The trackpad only has the first
/// three; X1 comes from shield_tx.py in receiver mode.
const MOUSE_BUTTONS: [(u8, MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u32); 4] = [
    (0x01, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
    (0x02, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
    (0x04, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
    (0x08, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
];
/// Every bit in MOUSE_BUTTONS
const MOUSE_BITS: u8 = 0x0f;

/// Set in `Gamepad::rumble` while a value is waiting to be taken
const RUMBLE_NEW: u32 = 1 << 16;

//region Gamepad

/// One controller's virtual Xbox 360 pad. Each has its own ViGEm client and XInput port.
pub struct Gamepad {
    /// Connected on the first plug, and kept for a replug
    client: Option<Client>,
    /// Plugged in from the controller's first setup until this is dropped
    pad: Option<Xbox360Wired<Client>>,
    /// Latest rumble a game set on the pad: large motor << 8 | small motor, plus RUMBLE_NEW.
    /// Written by the notification thread.
    rumble: Arc<AtomicU32>,
    /// Ends on its own once the pad is unplugged
    rumble_thread: Option<JoinHandle<()>>,
    /// The pad's XInput port, 1-4, once Windows has given it one
    port: Option<u8>,
}

impl Gamepad {
    /// Nothing's asked of ViGEm until plug
    pub fn new() -> Self {
        Gamepad { client: None, pad: None, rumble: Arc::new(AtomicU32::new(0)), rumble_thread: None, port: None }
    }

    /// Shows the virtual pad to Windows.
    pub fn plug(&mut self) -> Result<()> {
        if self.pad.is_some() {
            return Ok(());
        }
        // A target owns its client; ViGEm allows any number of connections
        let client = match self.client.take() {
            Some(c) => c,
            None => Client::connect().context("connecting to ViGEmBus; is it installed?")?,
        };
        let mut pad = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        pad.plugin().context("plugging in virtual Xbox 360 pad")?;
        pad.wait_ready().context("waiting for virtual pad")?;
        info!("virtual Xbox 360 pad plugged in");
        match pad.request_notification() {
            Ok(req) => {
                let slot = Arc::clone(&self.rumble);
                self.rumble_thread = Some(req.spawn_thread(move |_, n| {
                    let v = RUMBLE_NEW | u32::from(n.large_motor) << 8 | u32::from(n.small_motor);
                    slot.store(v, Ordering::Relaxed);
                }));
            }
            Err(e) => warn!("no rumble from the virtual pad: {e}"),
        }
        self.pad = Some(pad);
        Ok(())
    }

    /// Neutral, then removes the virtual pad.
    pub fn unplug(&mut self) {
        self.release();
        if let Some(mut pad) = self.pad.take() {
            if let Err(e) = pad.unplug() {
                warn!("unplugging virtual pad: {e}");
            }
            info!("virtual Xbox 360 pad unplugged");
        }
        // Unplugging aborts its pending notification, which ends the thread
        if let Some(t) = self.rumble_thread.take() {
            let _ = t.join();
        }
        self.rumble.store(0, Ordering::Relaxed);
        self.port = None;
    }

    /// Windows can see it
    pub fn plugged(&self) -> bool {
        self.pad.is_some()
    }

    /// The pad's XInput port, 1-4, asked of ViGEm until it has one
    pub fn port(&mut self) -> Option<u8> {
        if self.port.is_none()
            && let Some(pad) = self.pad.as_mut()
        {
            self.port = pad.get_user_index().ok().and_then(|i| u8::try_from(i + 1).ok());
        }
        self.port
    }

    /// The game's latest rumble, large and small motor, if it changed since the last call
    pub fn take_rumble(&self) -> Option<(u8, u8)> {
        let v = self.rumble.swap(0, Ordering::Relaxed);
        (v & RUMBLE_NEW != 0).then_some(((v >> 8) as u8, v as u8))
    }

    /// Neutral pad. For when input stops arriving.
    pub fn release(&mut self) {
        self.update(&XGamepad::default());
    }

    pub fn pad(&mut self, p: &Pad) {
        self.update(&to_xgamepad(p));
    }

    /// Raw XInput state, for senders that already did the mapping
    pub fn update(&mut self, g: &XGamepad) {
        if let Some(pad) = self.pad.as_mut()
            && let Err(e) = pad.update(g)
        {
            warn!("updating virtual pad: {e}");
        }
    }
}

impl Drop for Gamepad {
    fn drop(&mut self) {
        // Never leave a stick held
        self.unplug();
    }
}

//endregion

//region Desktop

/// Bits each source holds. Windows sees them all OR'd together, so a button stays down while
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
            send(&[mouse(ix as i32, iy as i32, 0, MOUSEEVENTF_MOVE)]);
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
        let inputs: Vec<INPUT> = MEDIA_KEYS
            .iter()
            .filter(|(bit, _)| changed & bit != 0)
            .map(|&(bit, vk)| keyboard(vk, keys & bit == 0))
            .collect();
        send(&inputs);
        if keys & REPEATING == 0 {
            self.next_repeat = None;
        } else if changed & keys & REPEATING != 0 {
            // Read per press, so a change in the keyboard settings applies right away
            let (delay, interval) = keyboard_repeat();
            self.next_repeat = Some((Instant::now() + delay, interval));
        }
    }

    /// Repeats held volume keys; call often, at least every repeat interval
    pub fn poll(&mut self, now: Instant) {
        let Some((at, interval)) = self.next_repeat else { return };
        if now < at {
            return;
        }
        let keys = self.keys.all();
        let inputs: Vec<INPUT> = MEDIA_KEYS
            .iter()
            .filter(|(bit, _)| keys & REPEATING & bit != 0)
            .map(|&(_, vk)| keyboard(vk, false))
            .collect();
        let late_ms = now.duration_since(at).as_millis() as u64;
        debug!(keys = format!("{keys:#04x}"), late_ms, "media key repeat");
        send(&inputs);
        // A stalled caller gets one repeat, not a burst
        self.next_repeat = Some(((at + interval).max(now), interval));
    }

    /// Mouse button bits as in MOUSE_BUTTONS; events go out on each edge of everyone's together
    pub fn set_mouse_buttons(&mut self, src: &Mac, buttons: u8) {
        let (was, buttons) = self.buttons.set(*src, buttons & MOUSE_BITS);
        let changed = was ^ buttons;
        let inputs: Vec<INPUT> = MOUSE_BUTTONS
            .iter()
            .filter(|(bit, ..)| changed & bit != 0)
            .map(|&(bit, down, up, data)| mouse(0, 0, data, if buttons & bit != 0 { down } else { up }))
            .collect();
        send(&inputs);
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

/// Controller pad -> XInput state. Pure, so it can be tested.
pub fn to_xgamepad(p: &Pad) -> XGamepad {
    use vigem_client::XButtons as X;
    let mut raw = 0u16;
    for (from, to) in [
        (btn::A, X::A),
        (btn::B, X::B),
        (btn::X, X::X),
        (btn::Y, X::Y),
        (btn::LB, X::LB),
        (btn::RB, X::RB),
        (btn::L3, X::LTHUMB),
        (btn::R3, X::RTHUMB),
        (btn::START, X::START),
    ] {
        if p.buttons & from != 0 {
            raw |= to;
        }
    }
    if p.keys & key::BACK != 0 {
        raw |= X::BACK;
    }
    // Home is Guide. The NVIDIA button stays the controller's own, for connecting and power.
    if p.keys & key::HOME != 0 {
        raw |= X::GUIDE;
    }
    raw |= match p.hat {
        0 => X::UP,
        1 => X::UP | X::RIGHT,
        2 => X::RIGHT,
        3 => X::DOWN | X::RIGHT,
        4 => X::DOWN,
        5 => X::DOWN | X::LEFT,
        6 => X::LEFT,
        7 => X::UP | X::LEFT,
        _ => 0,
    };
    XGamepad {
        buttons: XButtons { raw },
        left_trigger: (p.lt >> 8) as u8,
        right_trigger: (p.rt >> 8) as u8,
        thumb_lx: axis(p.lx, false),
        // HID Y grows downward, XInput Y grows upward
        thumb_ly: axis(p.ly, true),
        thumb_rx: axis(p.rx, false),
        thumb_ry: axis(p.ry, true),
    }
}

fn axis(v: u16, invert: bool) -> i16 {
    let c = i32::from(v) - 0x8000;
    let c = if invert { -c - 1 } else { c };
    c.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

//endregion

//region SendInput

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: inputs is a valid slice of fully initialized INPUT structs
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        warn!(sent, wanted = inputs.len(), "SendInput dropped events (UIPI, or a secure desktop)");
    }
}

/// Windows' keyboard repeat delay, 250-1000 ms, and interval, ~2.5-30 per second. Falls back
/// to the defaults, 500 ms and ~30/s, if it can't read them.
fn keyboard_repeat() -> (Duration, Duration) {
    let get = |action, default: u32| {
        let mut v = 0u32;
        // SAFETY: both actions write one u32 through pvparam
        let ok = unsafe {
            SystemParametersInfoW(
                action,
                0,
                Some(&raw mut v as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if ok.is_ok() { v } else { default }
    };
    repeat_timing(get(SPI_GETKEYBOARDDELAY, 1), get(SPI_GETKEYBOARDSPEED, 31))
}

/// SPI_GETKEYBOARDDELAY 0-3 and SPI_GETKEYBOARDSPEED 0-31 -> delay, interval
fn repeat_timing(delay: u32, speed: u32) -> (Duration, Duration) {
    let delay = Duration::from_millis(250 * (u64::from(delay.min(3)) + 1));
    let per_sec = 2.5 + f64::from(speed.min(31)) * (30.0 - 2.5) / 31.0;
    (delay, Duration::from_secs_f64(1.0 / per_sec))
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// A media key the way a keyboard sends it: extended, with its E0-prefixed scan code. A bare VK
/// without either gets fewer volume steps out of Windows than it was sent.
fn keyboard(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = match vk {
        VK_VOLUME_UP => 0x30,
        VK_VOLUME_DOWN => 0x2E,
        VK_VOLUME_MUTE => 0x20,
        _ => 0,
    };
    let mut flags = KEYEVENTF_EXTENDEDKEY;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;
    use vigem_client::XButtons as X;

    #[test]
    fn repeat_timing_matches_control_panel() {
        let (d, i) = repeat_timing(0, 0);
        assert_eq!(d, Duration::from_millis(250));
        assert_eq!(i.as_millis(), 400);
        let (d, i) = repeat_timing(3, 31);
        assert_eq!(d, Duration::from_millis(1000));
        assert_eq!(i.as_millis(), 33);
    }

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
    fn neutral_pad_is_neutral() {
        let g = to_xgamepad(&Pad::default());
        assert_eq!(g.buttons.raw, 0);
        assert_eq!((g.left_trigger, g.right_trigger), (0, 0));
        // 0x7FFF is one count below center
        assert!(g.thumb_lx.abs() <= 1 && g.thumb_ly.abs() <= 1);
    }

    #[test]
    fn buttons_keys_and_hat() {
        let p = Pad { buttons: btn::A | btn::Y | btn::START, keys: key::HOME | key::BACK, hat: 3, ..Pad::default() };
        let g = to_xgamepad(&p);
        assert_eq!(g.buttons.raw, X::A | X::Y | X::START | X::GUIDE | X::BACK | X::DOWN | X::RIGHT);
    }

    #[test]
    fn nvidia_button_maps_to_nothing() {
        let g = to_xgamepad(&Pad { keys: key::NVIDIA, ..Pad::default() });
        assert_eq!(g.buttons.raw, 0);
    }

    #[test]
    fn spurious_button_is_dropped() {
        let p = Pad { buttons: btn::SPURIOUS, ..Pad::default() };
        assert_eq!(to_xgamepad(&p).buttons.raw, 0);
    }

    #[test]
    fn axes_and_triggers() {
        let p = Pad { lx: 0, ly: 0, rx: 0xFFFF, ry: 0xFFFF, lt: 0xFFFF, rt: 0x8000, ..Pad::default() };
        let g = to_xgamepad(&p);
        assert_eq!(g.thumb_lx, i16::MIN);
        assert_eq!(g.thumb_ly, i16::MAX, "stick pushed up = HID 0 = XInput max");
        assert_eq!(g.thumb_rx, i16::MAX);
        assert_eq!(g.thumb_ry, i16::MIN);
        assert_eq!((g.left_trigger, g.right_trigger), (0xFF, 0x80));
    }
}
