//! The outputs on Linux, through uinput: each virtual pad a wired Xbox 360 pad as the kernel's
//! xpad driver has one, so SDL, Steam and the rest map it as they would the real thing; the
//! mouse and media keys one device shared by every controller. /dev/uinput needs the udev rule.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, BusType, EventSummary, FFEffectCode, FFEffectKind,
    InputEvent, InputId, KeyCode, KeyEvent, RelativeAxisCode, RelativeAxisEvent, UInputCode, UinputAbsSetup,
};
use shdlink::xb;
use tracing::{info, warn};

use super::{MediaKey, MouseButton, PadState, Rumble};

/// What the kernel's xpad driver calls a wired 360 pad
const PAD_NAME: &str = "Microsoft X-Box 360 pad";
/// Its USB IDs and bcdDevice, which SDL's mappings key on
const PAD_ID: (u16, u16, u16) = (0x045e, 0x028e, 0x0114);
/// Effects a game can have uploaded at once, as xpad allows
const FF_EFFECTS: u32 = 16;
/// The longest the rumble thread sleeps before looking for an unplug
const RUMBLE_POLL: Duration = Duration::from_millis(100);

/// XUSB bit -> xpad's button, in xpad's order: A, B, X, Y, the bumpers, Back, Start, Guide and the
/// stick clicks. xpad gives X and Y the codes the kernel also names BTN_NORTH and BTN_WEST.
const BUTTONS: [(u16, KeyCode); 11] = [
    (xb::A, KeyCode::BTN_SOUTH),
    (xb::B, KeyCode::BTN_EAST),
    (xb::X, KeyCode::BTN_NORTH),
    (xb::Y, KeyCode::BTN_WEST),
    (xb::LB, KeyCode::BTN_TL),
    (xb::RB, KeyCode::BTN_TR),
    (xb::BACK, KeyCode::BTN_SELECT),
    (xb::START, KeyCode::BTN_START),
    (xb::GUIDE, KeyCode::BTN_MODE),
    (xb::LTHUMB, KeyCode::BTN_THUMBL),
    (xb::RTHUMB, KeyCode::BTN_THUMBR),
];

/// The ports taken, bit n for port n + 1. A pad takes the lowest one free when it plugs in, as
/// XInput hands them out, so the tray's numbers mean the same thing on both.
static PORTS: AtomicU8 = AtomicU8::new(0);

fn take_port() -> Option<u8> {
    let mut taken = PORTS.load(Ordering::Acquire);
    loop {
        let n = (0..shdtray::MAX_CONTROLLERS as u8).find(|n| taken & 1 << n == 0)?;
        match PORTS.compare_exchange(taken, taken | 1 << n, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(n + 1),
            Err(now) => taken = now,
        }
    }
}

fn give_port(port: u8) {
    PORTS.fetch_and(!(1 << (port - 1)), Ordering::AcqRel);
}

//region Gamepad

/// One controller's virtual Xbox 360 pad
pub struct Gamepad {
    /// From the controller's first setup until it's unplugged. Shared with the rumble thread,
    /// which hears the game's effects on it.
    pad: Option<Arc<Mutex<VirtualDevice>>>,
    rumble: Rumble,
    rumble_thread: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    /// 1-4, from plug to unplug
    port: Option<u8>,
}

impl Gamepad {
    /// Nothing's asked of uinput until plug
    pub fn new() -> Self {
        Gamepad { pad: None, rumble: Rumble::default(), rumble_thread: None, port: None }
    }

    /// Shows the virtual pad to the system.
    pub fn plug(&mut self) -> Result<()> {
        if self.pad.is_some() {
            return Ok(());
        }
        let dev = Arc::new(Mutex::new(virtual_pad().context("making a virtual Xbox 360 pad")?));
        info!("virtual Xbox 360 pad plugged in");
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new().name("rumble".into()).spawn({
            let (dev, rumble, stop) = (Arc::clone(&dev), self.rumble.clone(), Arc::clone(&stop));
            move || hear_rumble(&dev, &rumble, &stop)
        });
        match thread {
            Ok(t) => self.rumble_thread = Some((stop, t)),
            Err(e) => warn!("no rumble from the virtual pad: {e}"),
        }
        self.pad = Some(dev);
        self.port = take_port();
        Ok(())
    }

    /// Neutral, then removes the virtual pad.
    pub fn unplug(&mut self) {
        self.release();
        if let Some((stop, t)) = self.rumble_thread.take() {
            stop.store(true, Ordering::Release);
            let _ = t.join();
        }
        // The device goes with its last reference, which is now this one
        if self.pad.take().is_some() {
            info!("virtual Xbox 360 pad unplugged");
        }
        if let Some(p) = self.port.take() {
            give_port(p);
        }
        self.rumble.clear();
    }

    /// The system can see it
    pub fn plugged(&self) -> bool {
        self.pad.is_some()
    }

    /// The pad's port, 1-4, while it's plugged in
    pub fn port(&mut self) -> Option<u8> {
        self.port
    }

    /// The game's latest rumble, large and small motor, if it changed since the last call
    pub fn take_rumble(&self) -> Option<(u8, u8)> {
        self.rumble.take()
    }

    pub fn update(&mut self, s: &PadState) {
        let Some(pad) = &self.pad else { return };
        // The kernel passes on only what changed
        let events = pad_events(s);
        if let Err(e) = pad.lock().map_err(|_| ErrorKind::Other.into()).and_then(|mut d| d.emit(&events)) {
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

/// A uinput device that looks like xpad's wired 360 pad: its buttons, sticks, triggers and
/// d-pad hat, and rumble
fn virtual_pad() -> std::io::Result<VirtualDevice> {
    let keys: AttributeSet<KeyCode> = BUTTONS.iter().map(|&(_, k)| k).collect();
    // xpad's ranges, fuzz and flat
    let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
    let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
    let mut b = VirtualDevice::builder()?
        .name(PAD_NAME)
        .input_id(InputId::new(BusType::BUS_USB, PAD_ID.0, PAD_ID.1, PAD_ID.2))
        .with_keys(&keys)?
        .with_ff(&AttributeSet::from_iter([FFEffectCode::FF_RUMBLE]))?
        .with_ff_effects_max(FF_EFFECTS);
    for (axis, info) in [
        (AbsoluteAxisCode::ABS_X, stick),
        (AbsoluteAxisCode::ABS_Y, stick),
        (AbsoluteAxisCode::ABS_RX, stick),
        (AbsoluteAxisCode::ABS_RY, stick),
        (AbsoluteAxisCode::ABS_Z, trigger),
        (AbsoluteAxisCode::ABS_RZ, trigger),
        (AbsoluteAxisCode::ABS_HAT0X, hat),
        (AbsoluteAxisCode::ABS_HAT0Y, hat),
    ] {
        b = b.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
    }
    let dev = b.build()?;
    // The rumble thread waits with poll, then reads what's there without blocking
    // SAFETY: plain fcntl on our own open fd
    unsafe {
        let fd = dev.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(dev)
}

/// `s` as xpad reports it. Its sticks' Y grows downward, the other way from XInput's.
fn pad_events(s: &PadState) -> Vec<InputEvent> {
    let held = |bit: u16| i32::from(s.buttons & bit != 0);
    let hat = |neg: u16, pos: u16| held(pos) - held(neg);
    let mut events: Vec<InputEvent> = BUTTONS.iter().map(|&(bit, k)| *KeyEvent::new(k, held(bit))).collect();
    events.extend([
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_X, i32::from(s.lx)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Y, i32::from(!s.ly)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_RX, i32::from(s.rx)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_RY, i32::from(!s.ry)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_Z, i32::from(s.lt)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_RZ, i32::from(s.rt)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_HAT0X, hat(xb::LEFT, xb::RIGHT)),
        *AbsoluteAxisEvent::new(AbsoluteAxisCode::ABS_HAT0Y, hat(xb::UP, xb::DOWN)),
    ]);
    events
}

/// The rumble effects a game has uploaded and is playing, as the motors should run
#[derive(Debug, Default)]
struct Effects {
    /// By effect ID: strong and weak magnitude, and how long one play lasts, 0 for until stopped
    uploaded: HashMap<i16, (u16, u16, Duration)>,
    /// By effect ID: until when, None for until stopped
    playing: HashMap<i16, Option<Instant>>,
}

impl Effects {
    fn upload(&mut self, id: i16, strong: u16, weak: u16, length: Duration) {
        self.uploaded.insert(id, (strong, weak, length));
    }

    fn erase(&mut self, id: i16) {
        self.uploaded.remove(&id);
        self.playing.remove(&id);
    }

    /// Played `count` times over, or stopped with 0
    fn play(&mut self, id: i16, count: i32, now: Instant) {
        let Some(&(.., length)) = self.uploaded.get(&id) else { return };
        if count <= 0 {
            self.playing.remove(&id);
        } else {
            let until = (!length.is_zero()).then(|| now + length * count as u32);
            self.playing.insert(id, until);
        }
    }

    /// Drops what's run its length; then the motors as the strongest of what's left has them,
    /// large and small, and when that next changes on its own
    fn motors(&mut self, now: Instant) -> ((u8, u8), Option<Instant>) {
        self.playing.retain(|_, until| until.is_none_or(|t| t > now));
        let mut out = (0u16, 0u16);
        for id in self.playing.keys() {
            if let Some(&(strong, weak, _)) = self.uploaded.get(id) {
                out = (out.0.max(strong), out.1.max(weak));
            }
        }
        let next = self.playing.values().flatten().min().copied();
        (((out.0 >> 8) as u8, (out.1 >> 8) as u8), next)
    }
}

/// Answers the game's uploads and erases, follows what it plays and stops, and passes the
/// motors on through `rumble`, until `stop`
fn hear_rumble(dev: &Mutex<VirtualDevice>, rumble: &Rumble, stop: &AtomicBool) {
    let fd = match dev.lock() {
        Ok(d) => d.as_raw_fd(),
        Err(_) => return,
    };
    let mut effects = Effects::default();
    let mut motors = (0, 0);
    while !stop.load(Ordering::Acquire) {
        let (_, next) = effects.motors(Instant::now());
        let wait = next.map_or(RUMBLE_POLL, |t| t.saturating_duration_since(Instant::now()).min(RUMBLE_POLL));
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, ours; the fd lives as long as the device, which outlives this thread
        unsafe { libc::poll(&mut pfd, 1, wait.as_millis() as i32) };
        if pfd.revents & libc::POLLIN != 0
            && let Ok(mut d) = dev.lock()
            && let Err(e) = take_effects(&mut d, &mut effects)
        {
            warn!("rumble: {e}");
            return;
        }
        let (now, _) = effects.motors(Instant::now());
        if now != motors {
            motors = now;
            rumble.set(now.0, now.1);
        }
    }
}

/// Everything waiting on the pad's fd, into `effects`
fn take_effects(d: &mut VirtualDevice, effects: &mut Effects) -> std::io::Result<()> {
    let events: Vec<InputEvent> = match d.fetch_events() {
        Ok(evs) => evs.collect(),
        Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(e),
    };
    for ev in events {
        match ev.destructure() {
            EventSummary::UInput(ev, UInputCode::UI_FF_UPLOAD, ..) => {
                // The kernel picks the ID; the game waits until this answer goes back
                let mut up = d.process_ff_upload(ev)?;
                let effect = up.effect();
                match effect.kind {
                    FFEffectKind::Rumble { strong_magnitude, weak_magnitude } => {
                        let length = Duration::from_millis(effect.replay.length.into());
                        effects.upload(up.effect_id(), strong_magnitude, weak_magnitude, length);
                        up.set_retval(0);
                    }
                    _ => up.set_retval(-libc::EINVAL),
                }
            }
            EventSummary::UInput(ev, UInputCode::UI_FF_ERASE, ..) => {
                let erase = d.process_ff_erase(ev)?;
                effects.erase(erase.effect_id() as i16);
            }
            EventSummary::ForceFeedback(_, id, count) => effects.play(id.0 as i16, count, Instant::now()),
            _ => {}
        }
    }
    Ok(())
}

//endregion

//region Desktop

/// The mouse and media keys: one uinput device for every controller, made the first time it's
/// needed. Without /dev/uinput, input goes nowhere, said once.
pub(super) struct Input {
    dev: Option<VirtualDevice>,
    failed: bool,
}

impl Input {
    pub(super) fn new() -> Self {
        Input { dev: None, failed: false }
    }

    pub(super) fn move_by(&mut self, dx: i32, dy: i32) {
        self.emit(&[
            *RelativeAxisEvent::new(RelativeAxisCode::REL_X, dx),
            *RelativeAxisEvent::new(RelativeAxisCode::REL_Y, dy),
        ]);
    }

    /// Each button down or up
    pub(super) fn buttons(&mut self, edges: &[(MouseButton, bool)]) {
        let events: Vec<InputEvent> = edges.iter().map(|&(b, down)| *KeyEvent::new(button(b), down.into())).collect();
        self.emit(&events);
    }

    /// Each key down or up
    pub(super) fn keys(&mut self, edges: &[(MediaKey, bool)]) {
        let events: Vec<InputEvent> = edges.iter().map(|&(k, down)| *KeyEvent::new(media(k), down.into())).collect();
        self.emit(&events);
    }

    /// None: the compositor or X server repeats a held key from any keyboard, ours included
    pub(super) fn repeat() -> Option<(Duration, Duration)> {
        None
    }

    fn emit(&mut self, events: &[InputEvent]) {
        if events.is_empty() || self.failed {
            return;
        }
        if self.dev.is_none() {
            match desktop_device() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    self.failed = true;
                    return warn!("no mouse or media keys from the controllers: {e:#}");
                }
            }
        }
        if let Some(d) = &mut self.dev
            && let Err(e) = d.emit(events)
        {
            warn!("mouse or media keys: {e}");
        }
    }
}

fn button(b: MouseButton) -> KeyCode {
    match b {
        MouseButton::Left => KeyCode::BTN_LEFT,
        MouseButton::Right => KeyCode::BTN_RIGHT,
        MouseButton::Middle => KeyCode::BTN_MIDDLE,
        MouseButton::X1 => KeyCode::BTN_SIDE,
    }
}

fn media(k: MediaKey) -> KeyCode {
    match k {
        MediaKey::VolumeUp => KeyCode::KEY_VOLUMEUP,
        MediaKey::VolumeDown => KeyCode::KEY_VOLUMEDOWN,
        MediaKey::Mute => KeyCode::KEY_MUTE,
    }
}

fn desktop_device() -> Result<VirtualDevice> {
    let keys: AttributeSet<KeyCode> = [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::X1]
        .map(button)
        .into_iter()
        .chain([MediaKey::VolumeUp, MediaKey::VolumeDown, MediaKey::Mute].map(media))
        .collect();
    let axes = AttributeSet::from_iter([RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y]);
    let dev = VirtualDevice::builder()
        .context("opening /dev/uinput; is shieldd's udev rule installed?")?
        .name("shieldd mouse and media keys")
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()
        .context("making the mouse and media keys")?;
    info!("virtual mouse and media keys up");
    Ok(dev)
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y_grows_downward_and_the_hat_is_signed() {
        let s = PadState { buttons: xb::A | xb::UP | xb::RIGHT, ly: i16::MAX, ry: -1, lt: 200, ..PadState::default() };
        let events = pad_events(&s);
        let value = |code: AbsoluteAxisCode| {
            events.iter().find(|e| e.event_type() == evdev::EventType::ABSOLUTE && e.code() == code.0).unwrap().value()
        };
        assert_eq!(value(AbsoluteAxisCode::ABS_Y), i32::from(i16::MIN), "pushed up is xpad's minimum");
        assert_eq!(value(AbsoluteAxisCode::ABS_RY), 0);
        assert_eq!(value(AbsoluteAxisCode::ABS_Z), 200);
        assert_eq!((value(AbsoluteAxisCode::ABS_HAT0X), value(AbsoluteAxisCode::ABS_HAT0Y)), (1, -1));
        let down: Vec<u16> = events
            .iter()
            .filter(|e| e.event_type() == evdev::EventType::KEY && e.value() == 1)
            .map(|e| e.code())
            .collect();
        assert_eq!(down, [KeyCode::BTN_SOUTH.0]);
    }

    #[test]
    fn every_button_has_its_own_code() {
        let mut bits: Vec<u16> = BUTTONS.iter().map(|b| b.0).collect();
        let mut codes: Vec<u16> = BUTTONS.iter().map(|b| b.1.0).collect();
        bits.dedup();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!((bits.len(), codes.len()), (BUTTONS.len(), BUTTONS.len()));
    }

    #[test]
    fn effects_play_for_their_length() {
        let t0 = Instant::now();
        let mut fx = Effects::default();
        fx.upload(0, 0xFFFF, 0x1000, Duration::from_millis(200));
        fx.upload(1, 0x8000, 0x8000, Duration::ZERO);
        assert_eq!(fx.motors(t0), ((0, 0), None), "uploaded isn't playing");
        fx.play(0, 1, t0);
        assert_eq!(fx.motors(t0), ((0xFF, 0x10), Some(t0 + Duration::from_millis(200))));
        fx.play(1, 1, t0);
        assert_eq!(fx.motors(t0).0, (0xFF, 0x80), "the strongest of each");
        assert_eq!(fx.motors(t0 + Duration::from_millis(250)), ((0x80, 0x80), None), "0 lasts until stopped");
        fx.play(1, 0, t0);
        assert_eq!(fx.motors(t0).0, (0, 0));
        fx.play(0, 3, t0);
        assert!(fx.motors(t0 + Duration::from_millis(500)).0 != (0, 0), "three plays");
        fx.erase(0);
        assert_eq!(fx.motors(t0).0, (0, 0));
        fx.play(7, 1, t0);
        assert_eq!(fx.motors(t0).0, (0, 0), "never uploaded");
    }

    /// A real pad through /dev/uinput: the kernel lists it as xpad's, and a game's rumble on its
    /// event node comes back out of take_rumble. Needs read-write on /dev/uinput and on the
    /// event node it makes: `cargo test -p shieldd uinput -- --ignored`
    #[test]
    #[ignore = "needs /dev/uinput and the event node it makes"]
    fn uinput_pad_end_to_end() {
        use evdev::{FFEffectData, FFReplay, FFTrigger};
        let mut g = Gamepad::new();
        g.plug().expect("plugging a pad; is /dev/uinput open to this user?");
        assert!(g.port().is_some());
        let node = {
            let mut d = g.pad.as_ref().unwrap().lock().unwrap();
            d.enumerate_dev_nodes_blocking().unwrap().find_map(|p| p.ok()).expect("its event node")
        };
        let listed = std::fs::read_to_string("/proc/bus/input/devices").unwrap();
        let ours = listed.split("\n\n").find(|d| d.contains(node.file_name().unwrap().to_str().unwrap())).unwrap();
        assert!(ours.contains("Vendor=045e Product=028e Version=0114") && ours.contains(PAD_NAME), "{ours}");

        g.update(&PadState { buttons: xb::A, ..PadState::default() });
        // As a game would
        let mut game = evdev::Device::open(&node).expect("opening its event node; is it open to this user?");
        let data = FFEffectData {
            direction: 0,
            trigger: FFTrigger::default(),
            replay: FFReplay { length: 300, delay: 0 },
            kind: FFEffectKind::Rumble { strong_magnitude: 0xFFFF, weak_magnitude: 0x4000 },
        };
        let mut effect = game.upload_ff_effect(data).unwrap();
        effect.play(1).unwrap();
        let heard = |want: (u8, u8)| {
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until {
                if let Some(r) = g.take_rumble() {
                    return r == want;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        };
        assert!(heard((0xFF, 0x40)), "the game's rumble");
        // Its 300 ms run out on their own
        assert!(heard((0, 0)), "back off at the end of its length");
        drop(effect);
        drop(game);
        g.unplug();
        assert!(!g.plugged());
    }

    #[test]
    fn ports_go_lowest_first() {
        // Shared with any other test plugging pads; none do
        let (a, b) = (take_port().unwrap(), take_port().unwrap());
        assert_eq!((a, b), (1, 2));
        give_port(a);
        assert_eq!(take_port(), Some(1));
        give_port(1);
        give_port(2);
    }
}
