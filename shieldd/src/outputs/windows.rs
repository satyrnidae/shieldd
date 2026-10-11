//! The outputs on Windows: each virtual pad through ViGEmBus, the mouse and media keys through
//! SendInput.

use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};
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

use super::{MediaKey, MouseButton, PadState, Rumble};

const XBUTTON1: u32 = 0x0001;

//region Gamepad

/// One controller's virtual Xbox 360 pad. Each has its own ViGEm client and XInput port.
pub struct Gamepad {
    /// Connected on the first plug, and kept for a replug
    client: Option<Client>,
    /// Plugged in from the controller's first setup until this is dropped
    pad: Option<Xbox360Wired<Client>>,
    /// Written by the notification thread
    rumble: Rumble,
    /// Ends on its own once the pad is unplugged
    rumble_thread: Option<JoinHandle<()>>,
    /// The pad's XInput port, 1-4, once Windows has given it one
    port: Option<u8>,
}

impl Gamepad {
    /// Nothing's asked of ViGEm until plug
    pub fn new() -> Self {
        Gamepad { client: None, pad: None, rumble: Rumble::default(), rumble_thread: None, port: None }
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
                let rumble = self.rumble.clone();
                self.rumble_thread = Some(req.spawn_thread(move |_, n| rumble.set(n.large_motor, n.small_motor)));
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
        self.rumble.clear();
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
        self.rumble.take()
    }

    pub fn update(&mut self, s: &PadState) {
        let g = XGamepad {
            buttons: XButtons { raw: s.buttons },
            left_trigger: s.lt,
            right_trigger: s.rt,
            thumb_lx: s.lx,
            thumb_ly: s.ly,
            thumb_rx: s.rx,
            thumb_ry: s.ry,
        };
        if let Some(pad) = self.pad.as_mut()
            && let Err(e) = pad.update(&g)
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

//region SendInput

/// The mouse and media keys, through SendInput
pub(super) struct Input;

impl Input {
    pub(super) fn new() -> Self {
        Input
    }

    pub(super) fn move_by(&mut self, dx: i32, dy: i32) {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]);
    }

    /// Each button down or up
    pub(super) fn buttons(&mut self, edges: &[(MouseButton, bool)]) {
        let inputs: Vec<INPUT> = edges
            .iter()
            .map(|&(b, down)| {
                let (d, u, data) = match b {
                    MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                    MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                    MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                    MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
                };
                mouse(0, 0, data, if down { d } else { u })
            })
            .collect();
        send(&inputs);
    }

    /// Each key down or up; a repeat is another down
    pub(super) fn keys(&mut self, edges: &[(MediaKey, bool)]) {
        let inputs: Vec<INPUT> = edges
            .iter()
            .map(|&(k, down)| {
                let vk = match k {
                    MediaKey::VolumeUp => VK_VOLUME_UP,
                    MediaKey::VolumeDown => VK_VOLUME_DOWN,
                    MediaKey::Mute => VK_VOLUME_MUTE,
                };
                keyboard(vk, !down)
            })
            .collect();
        send(&inputs);
    }

    /// Windows doesn't repeat injected keys, so Desktop does, at the keyboard's delay and rate
    pub(super) fn repeat() -> Option<(Duration, Duration)> {
        Some(keyboard_repeat())
    }
}

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

    #[test]
    fn repeat_timing_matches_control_panel() {
        let (d, i) = repeat_timing(0, 0);
        assert_eq!(d, Duration::from_millis(250));
        assert_eq!(i.as_millis(), 400);
        let (d, i) = repeat_timing(3, 31);
        assert_eq!(d, Duration::from_millis(1000));
        assert_eq!(i.as_millis(), 33);
    }

    /// shdlink's bits are XUSB's, so a PadState's go to ViGEm as they are
    #[test]
    fn xusb_bits_match_vigem() {
        use shdlink::xb;
        let pairs = [
            (xb::UP, XButtons::UP),
            (xb::DOWN, XButtons::DOWN),
            (xb::LEFT, XButtons::LEFT),
            (xb::RIGHT, XButtons::RIGHT),
            (xb::START, XButtons::START),
            (xb::BACK, XButtons::BACK),
            (xb::LTHUMB, XButtons::LTHUMB),
            (xb::RTHUMB, XButtons::RTHUMB),
            (xb::LB, XButtons::LB),
            (xb::RB, XButtons::RB),
            (xb::GUIDE, XButtons::GUIDE),
            (xb::A, XButtons::A),
            (xb::B, XButtons::B),
            (xb::X, XButtons::X),
            (xb::Y, XButtons::Y),
        ];
        assert!(pairs.iter().all(|(a, b)| a == b));
    }
}
