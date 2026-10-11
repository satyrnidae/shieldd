//! `--transmit` (Linux/WSL): read the controller's evdev + hidraw nodes, as
//! ozwpan enumerates it, and send its state as `SHD2` packets to a
//! `--receiver` on Windows. Replaces shield_tx.py, behaviour for behaviour.
//!
//! The pad shows up as several evdev nodes (gamepad, trackpad "Mouse", misc).
//! All of them are opened + grabbed and events are handled by code, so it
//! doesn't matter which node a key arrives on. The trackpad is read from
//! hidraw instead (see TP_*).
//!
//! Talks to evdev through its ioctls directly (linux/input.h) rather than a
//! crate; it needs only a handful of them.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use shdlink::{Packet, media, mouse, xb};
use tracing::{info, warn};

/// Resend state this often even if idle
const KEEPALIVE: Duration = Duration::from_millis(100);
const NVIDIA: u16 = 0x0955;

// linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_MSC: u16 = 0x04;
const MSC_SCAN: u16 = 0x04;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_SOUTH: u16 = 0x130;
const BTN_EAST: u16 = 0x131;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_TL2: u16 = 0x138;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
const KEY_MUTE: u16 = 113;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;
const KEY_POWER: u16 = 116;
const KEY_BACK: u16 = 158;
const KEY_HOMEPAGE: u16 = 172;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RZ: u16 = 0x05;
const ABS_GAS: u16 = 0x09;
const ABS_BRAKE: u16 = 0x0a;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

// trackpad: hid report 0x02 = [id, flags, x lo, x hi, y lo, y hi]. x/y are the
// absolute finger position, 16 bit, but the descriptor declares them as relative
// int8, so evdev's REL_X/REL_Y are positions clamped at 127 (cursor drifts, right
// half of the pad dead). read hidraw and turn position changes into motion.
const TP_REPORT: u8 = 0x02;
/// Finger down (the evdev BTN_EXTRA)
const TP_TOUCH: u8 = 0x08;
/// 0x01 = pad click
const TP_BTNS: [(u8, u8); 3] = [(0x01, mouse::LEFT), (0x02, mouse::RIGHT), (0x04, mouse::MIDDLE)];

/// evdev key -> (xusb bit, name). Verify with --dump and edit
const BTN_MAP: &[(u16, u16, &str)] = &[
    (BTN_SOUTH, xb::A, "A"),
    (BTN_EAST, xb::B, "B"),
    (BTN_NORTH, xb::X, "X"),
    (BTN_WEST, xb::Y, "Y"),
    (BTN_TL, xb::LB, "LB"),
    (BTN_TR, xb::RB, "RB"),
    (BTN_SELECT, xb::BACK, "BACK"),
    (BTN_START, xb::START, "START"),
    (BTN_MODE, xb::GUIDE, "GUIDE"),
    (BTN_THUMBL, xb::LTHUMB, "LTHUMB"),
    (BTN_THUMBR, xb::RTHUMB, "RTHUMB"),
    // capacitive buttons come in as consumer keys on this pad
    (KEY_BACK, xb::BACK, "BACK"),       // back -> select
    (KEY_HOMEPAGE, xb::GUIDE, "GUIDE"), // home
];
const MEDIA_MAP: &[(u16, u8, &str)] = &[
    (KEY_VOLUMEUP, media::VOL_UP, "VOL_UP"),
    (KEY_VOLUMEDOWN, media::VOL_DOWN, "VOL_DOWN"),
    (KEY_MUTE, media::MUTE, "MUTE"),
];
/// Dropped silently:
/// - BTN_TL2 (hid button 9) fires along with every volume key and trackpad click
/// - trackpad buttons/touch on the "Mouse" node: handled from hidraw instead
/// - KEY_POWER, the NVIDIA button: the controller's own, for connecting and power; Home is Guide
const IGNORE: &[u16] = &[BTN_TL2, BTN_EXTRA, BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, KEY_POWER];

#[derive(Clone, Copy)]
enum Axis {
    Lx,
    Ly,
    Rx,
    Ry,
    Lt,
    Rt,
}
/// abs code -> (field, invert)
const AXIS_MAP: &[(u16, Axis, bool)] = &[
    (ABS_X, Axis::Lx, false),
    (ABS_Y, Axis::Ly, true),
    (ABS_Z, Axis::Rx, false),
    (ABS_RZ, Axis::Ry, true),
    (ABS_BRAKE, Axis::Lt, false),
    (ABS_GAS, Axis::Rt, false),
];

//region evdev ioctls (linux/input.h; x86_64 _IOC layout)

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'E' as u64) << 8) | nr
}
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const EVIOCGID: u64 = ioc(IOC_READ, 0x02, 8);
const fn eviocgname(len: u64) -> u64 {
    ioc(IOC_READ, 0x06, len)
}
const fn eviocgbit(ev: u64, len: u64) -> u64 {
    ioc(IOC_READ, 0x20 + ev, len)
}
const fn eviocgabs(abs: u64) -> u64 {
    ioc(IOC_READ, 0x40 + abs, 24)
}
const EVIOCGRAB: u64 = ioc(IOC_WRITE, 0x90, 4);

/// struct input_event on 64-bit: timeval (16) + type + code + value
const EVENT_LEN: usize = 24;

#[derive(Clone, Copy)]
struct AbsInfo {
    min: i32,
    max: i32,
}

/// An open evdev or hidraw node; closing it also drops an evdev grab.
struct Node {
    path: String,
    name: String,
    fd: RawFd,
}

impl Drop for Node {
    fn drop(&mut self) {
        // SAFETY: fd is ours and closed only here
        unsafe { libc::close(self.fd) };
    }
}

impl Node {
    fn open(path: &Path) -> io::Result<Node> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: c is a valid nul-terminated path
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Node { path: path.display().to_string(), name: String::new(), fd })
    }

    fn ioctl_buf(&self, req: u64, buf: &mut [u8]) -> io::Result<()> {
        // SAFETY: buf is valid and writable for the size encoded in req
        let r = unsafe { libc::ioctl(self.fd, req as _, buf.as_mut_ptr()) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn vendor(&self) -> Option<u16> {
        let mut id = [0u8; 8]; // struct input_id: bustype, vendor, product, version
        self.ioctl_buf(EVIOCGID, &mut id).ok()?;
        Some(u16::from_ne_bytes([id[2], id[3]]))
    }

    fn device_name(&self) -> String {
        let mut name = [0u8; 256];
        let _ = self.ioctl_buf(eviocgname(name.len() as u64), &mut name);
        String::from_utf8_lossy(name.split(|&c| c == 0).next().unwrap_or_default()).into_owned()
    }

    fn has_abs(&self, code: u16) -> bool {
        let mut bits = [0u8; 8]; // ABS_MAX is 0x3f
        self.ioctl_buf(eviocgbit(EV_ABS as u64, bits.len() as u64), &mut bits).is_ok()
            && bits[(code / 8) as usize] & (1 << (code % 8)) != 0
    }

    fn absinfo(&self, code: u16) -> Option<AbsInfo> {
        // struct input_absinfo: value, minimum, maximum, fuzz, flat, resolution
        let mut b = [0u8; 24];
        self.ioctl_buf(eviocgabs(code as u64), &mut b).ok()?;
        let i = |o: usize| i32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
        Some(AbsInfo { min: i(4), max: i(8) })
    }

    fn grab(&self) -> io::Result<()> {
        // SAFETY: EVIOCGRAB takes an int by value
        let r = unsafe { libc::ioctl(self.fd, EVIOCGRAB as _, 1 as libc::c_int) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// Reads whatever is waiting; 0 when nothing is.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: buf is valid and writable for buf.len() bytes
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock { Ok(0) } else { Err(e) }
    }
}

/// (type, code, value) of one struct input_event
fn parse_event(ev: &[u8]) -> (u16, u16, i32) {
    (
        u16::from_ne_bytes([ev[16], ev[17]]),
        u16::from_ne_bytes([ev[18], ev[19]]),
        i32::from_ne_bytes(ev[20..24].try_into().unwrap()),
    )
}

/// Indexes of the nodes that are readable, or none after `timeout`
fn poll(nodes: &[&Node], timeout: Option<Duration>) -> Result<Vec<usize>> {
    let mut pfds: Vec<libc::pollfd> =
        nodes.iter().map(|n| libc::pollfd { fd: n.fd, events: libc::POLLIN, revents: 0 }).collect();
    let ms = timeout.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as i32);
    // SAFETY: pfds is a valid array of pfds.len() pollfd structs
    let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(Vec::new());
        }
        return Err(e).context("poll");
    }
    Ok(pfds.iter().enumerate().filter(|(_, p)| p.revents != 0).map(|(i, _)| i).collect())
}

//endregion

fn find_pad_nodes() -> Result<Vec<Node>> {
    let mut paths: Vec<PathBuf> = fs::read_dir("/dev/input")
        .context("listing /dev/input")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.as_bytes().starts_with(b"event")))
        .collect();
    paths.sort_by_key(|p| p.file_name().and_then(|n| n.to_str()?[5..].parse::<u32>().ok()).unwrap_or(u32::MAX));

    let mut nodes = Vec::new();
    let mut any_readable = false;
    for p in paths {
        let Ok(mut node) = Node::open(&p) else {
            continue;
        };
        any_readable = true;
        if node.vendor() == Some(NVIDIA) {
            node.name = node.device_name();
            nodes.push(node);
        }
    }
    if !any_readable {
        bail!("can't read /dev/input/event*. run with sudo or add yourself to group 'input'");
    }
    // gamepad node can lag the others; bail (systemd retries) until it's there
    if !nodes.iter().any(|n| n.has_abs(ABS_X)) {
        bail!("no 0955:* gamepad node. is the pad enumerated? check lsusb / evtest");
    }
    Ok(nodes)
}

/// hidraw node of the pad (same one shield-tx@ is started for)
fn find_hidraw() -> Result<PathBuf> {
    let mut entries: Vec<String> = fs::read_dir("/sys/class/hidraw")
        .context("listing /sys/class/hidraw")?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    for h in entries {
        if let Ok(u) = fs::read_to_string(format!("/sys/class/hidraw/{h}/device/uevent"))
            && u.contains("HID_ID=0003:00000955:00007210")
        {
            return Ok(PathBuf::from(format!("/dev/{h}")));
        }
    }
    bail!("no 0955:7210 hidraw node")
}

/// Scale raw to -1..1 (sticks) or 0..1 (triggers)
fn norm(v: i32, info: AbsInfo, trigger: bool) -> f32 {
    let (lo, hi) = (info.min as f32, info.max as f32);
    let t = if hi != lo { (v as f32 - lo) / (hi - lo) } else { 0.0 };
    if trigger { t } else { t * 2.0 - 1.0 }
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

fn key_name(code: u16) -> String {
    let name = match code {
        BTN_SOUTH => "BTN_SOUTH",
        BTN_EAST => "BTN_EAST",
        BTN_NORTH => "BTN_NORTH",
        BTN_WEST => "BTN_WEST",
        BTN_TL => "BTN_TL",
        BTN_TR => "BTN_TR",
        BTN_TL2 => "BTN_TL2",
        BTN_SELECT => "BTN_SELECT",
        BTN_START => "BTN_START",
        BTN_MODE => "BTN_MODE",
        BTN_THUMBL => "BTN_THUMBL",
        BTN_THUMBR => "BTN_THUMBR",
        BTN_LEFT => "BTN_LEFT",
        BTN_RIGHT => "BTN_RIGHT",
        BTN_MIDDLE => "BTN_MIDDLE",
        BTN_SIDE => "BTN_SIDE",
        BTN_EXTRA => "BTN_EXTRA",
        KEY_BACK => "KEY_BACK",
        KEY_HOMEPAGE => "KEY_HOMEPAGE",
        KEY_POWER => "KEY_POWER",
        KEY_VOLUMEUP => "KEY_VOLUMEUP",
        KEY_VOLUMEDOWN => "KEY_VOLUMEDOWN",
        KEY_MUTE => "KEY_MUTE",
        _ => return format!("{code:#x}"),
    };
    name.into()
}

fn type_name(t: u16) -> &'static str {
    match t {
        EV_SYN => "EV_SYN",
        EV_KEY => "EV_KEY",
        EV_REL => "EV_REL",
        EV_ABS => "EV_ABS",
        EV_MSC => "EV_MSC",
        _ => "EV_?",
    }
}

fn code_name(t: u16, code: u16) -> String {
    if t == EV_KEY { key_name(code) } else { format!("{code:#x}") }
}

/// Devices are grabbed, so --dump can't see them while this runs; log anything
/// not mapped (shows up in journalctl -u 'shield-tx@*').
fn unmapped(node: &Node, t: u16, code: u16, value: i32, usage: Option<i32>) {
    let hid = match usage {
        Some(u) if t == EV_KEY => format!(" hid usage {u:#x}"),
        _ => String::new(),
    };
    info!("unmapped {} {} {} = {value}{hid}", node.path, type_name(t), code_name(t, code));
}

struct Sender {
    sock: UdpSocket,
    dest: SocketAddr,
    p: Packet,
    /// evdev code -> xusb bit, so home + nvidia (both GUIDE) don't clear each other
    held: HashMap<u16, u16>,
    hat: [i32; 2],
    /// Trackpad motion since the last packet
    dx: i32,
    dy: i32,
    last: Instant,
}

impl Sender {
    fn set(&mut self, axis: Axis, v: f32) {
        let p = &mut self.p;
        match axis {
            Axis::Lx => p.lx = v,
            Axis::Ly => p.ly = v,
            Axis::Rx => p.rx = v,
            Axis::Ry => p.ry = v,
            Axis::Lt => p.lt = v,
            Axis::Rt => p.rt = v,
        }
    }

    fn send(&mut self) {
        let mut buttons = self.held.values().fold(0, |b, &bit| b | bit);
        buttons |= match self.hat[0].signum() {
            -1 => xb::LEFT,
            1 => xb::RIGHT,
            _ => 0,
        };
        buttons |= match self.hat[1].signum() {
            -1 => xb::UP,
            1 => xb::DOWN,
            _ => 0,
        };
        self.p.buttons = buttons;
        self.p.dx = clamp16(self.dx);
        self.p.dy = clamp16(self.dy);
        if let Err(e) = self.sock.send_to(&self.p.encode(), self.dest) {
            warn!("send to {}: {e}", self.dest);
        }
        self.dx = 0;
        self.dy = 0;
        self.p.seq = self.p.seq.wrapping_add(1);
        self.last = Instant::now();
    }
}

pub fn run(dest: SocketAddr, hidraw: Option<&Path>, dump_only: bool) -> Result<()> {
    let nodes = find_pad_nodes()?;
    for n in &nodes {
        info!("using {}: {}", n.path, n.name);
    }
    let hidraw = match hidraw {
        Some(p) => p.to_path_buf(),
        None => find_hidraw()?,
    };
    // hidraw isn't affected by evdev grabs
    let hid = Node::open(&hidraw).with_context(|| format!("opening {}", hidraw.display()))?;
    info!("using {} for the trackpad", hid.path);

    // poll set: the evdev nodes, then hidraw last
    let mut all: Vec<&Node> = nodes.iter().collect();
    all.push(&hid);
    let hid_i = all.len() - 1;
    let mut buf = [0u8; EVENT_LEN * 64];

    if dump_only {
        return dump(&all, hid_i, &mut buf);
    }

    let mut abs: HashMap<u16, AbsInfo> = HashMap::new();
    for n in &nodes {
        for &(code, _, _) in AXIS_MAP {
            if let Some(info) = n.absinfo(code) {
                abs.insert(code, info);
            }
        }
    }

    let sock = UdpSocket::bind("0.0.0.0:0").context("udp socket")?;
    info!(%dest, "sending SHD2 packets");
    let mut tx = Sender {
        sock,
        dest,
        p: Packet::default(),
        held: HashMap::new(),
        hat: [0, 0],
        dx: 0,
        dy: 0,
        last: Instant::now() - KEEPALIVE,
    };
    // last trackpad position while touching; None = finger up
    let mut tp_prev: Option<(u16, u16)> = None;
    // node index -> last MSC_SCAN (hid usage) seen on that node
    let mut scan: HashMap<usize, i32> = HashMap::new();

    for n in &nodes {
        // keep other wsl readers off it; the grab goes when the fd closes
        if let Err(e) = n.grab() {
            warn!("grab {}: {e}", n.path);
        }
    }

    loop {
        // wait with timeout so keepalive still fires when idle
        let ready = poll(&all, Some(KEEPALIVE.saturating_sub(tx.last.elapsed())))?;
        if ready.is_empty() {
            tx.send();
            continue;
        }
        for i in ready {
            let node = all[i];
            // the pad going away (ENODEV) ends the process; systemd stops/restarts it
            let n = node.read(&mut buf).with_context(|| format!("reading {}", node.path))?;
            if i == hid_i {
                let rep = &buf[..n];
                if rep.len() < 6 || rep[0] != TP_REPORT {
                    continue; // gamepad reports come via evdev
                }
                let flags = rep[1];
                let (x, y) = (u16::from_le_bytes([rep[2], rep[3]]), u16::from_le_bytes([rep[4], rep[5]]));
                if flags & TP_TOUCH != 0 {
                    // first sample of a touch only sets the origin, so
                    // putting a finger down doesn't jump the cursor
                    if let Some((px, py)) = tp_prev {
                        tx.dx += i32::from(x) - i32::from(px);
                        tx.dy += i32::from(y) - i32::from(py);
                    }
                    tp_prev = Some((x, y));
                } else {
                    tp_prev = None;
                }
                let new = TP_BTNS.iter().filter(|&&(bit, _)| flags & bit != 0).fold(0, |m, &(_, mb)| m | mb);
                if new & !tx.p.mouse_buttons != 0 {
                    info!("key {} trackpad {flags:#04x} -> mouse", node.path);
                }
                tx.p.mouse_buttons = new;
                tx.send();
                continue;
            }
            for ev in buf[..n].as_chunks::<EVENT_LEN>().0 {
                let (t, code, value) = parse_event(ev);
                match t {
                    EV_MSC if code == MSC_SCAN => {
                        scan.insert(i, value); // hid usage of the key that follows
                    }
                    EV_KEY => {
                        if value == 2 || IGNORE.contains(&code) {
                            continue; // autorepeat / not a button
                        }
                        let btn = BTN_MAP.iter().find(|&&(c, _, _)| c == code);
                        let med = MEDIA_MAP.iter().find(|&&(c, _, _)| c == code);
                        // mapped presses too, so 'did it fire at all' is answerable from the journal
                        if value != 0
                            && let Some(target) = btn.map(|b| b.2).or(med.map(|m| m.2))
                        {
                            info!("key {} {} -> {target}", node.path, key_name(code));
                        }
                        if let Some(&(_, bit, _)) = btn {
                            if value != 0 {
                                tx.held.insert(code, bit);
                            } else {
                                tx.held.remove(&code);
                            }
                        } else if let Some(&(_, bit, _)) = med {
                            if value != 0 {
                                tx.p.media |= bit;
                            } else {
                                tx.p.media &= !bit;
                            }
                        } else {
                            unmapped(node, t, code, value, scan.get(&i).copied());
                        }
                    }
                    EV_REL => {} // clamped positions, trackpad comes from hidraw
                    EV_ABS if code == ABS_HAT0X => tx.hat[0] = value,
                    EV_ABS if code == ABS_HAT0Y => tx.hat[1] = value,
                    EV_ABS => match (AXIS_MAP.iter().find(|&&(c, _, _)| c == code), abs.get(&code)) {
                        (Some(&(_, axis, inv)), Some(&info)) => {
                            let v = norm(value, info, matches!(axis, Axis::Lt | Axis::Rt));
                            tx.set(axis, if inv { -v } else { v });
                        }
                        _ => unmapped(node, t, code, value, None),
                    },
                    EV_SYN => tx.send(), // one packet per input frame
                    _ => unmapped(node, t, code, value, scan.get(&i).copied()),
                }
            }
        }
    }
}

/// --dump: print raw events from every pad node (ungrabbed), to fix the maps above
fn dump(all: &[&Node], hid_i: usize, buf: &mut [u8]) -> Result<()> {
    loop {
        for i in poll(all, None)? {
            let node = all[i];
            let n = node.read(buf).with_context(|| format!("reading {}", node.path))?;
            if i == hid_i {
                if n > 0 && buf[0] != 0x01 {
                    // skip gamepad reports, evdev shows those
                    println!("{} {}", node.path, crate::hex(&buf[..n]));
                }
                continue;
            }
            for ev in buf[..n].as_chunks::<EVENT_LEN>().0 {
                let (t, code, value) = parse_event(ev);
                if matches!(t, EV_KEY | EV_ABS | EV_REL) {
                    println!("{} {} {} {value}", node.path, type_name(t), code_name(t, code));
                }
            }
        }
    }
}
