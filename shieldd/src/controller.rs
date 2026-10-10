//! The controller at the USB level, over Ozmo: configures it after each
//! connect, runs the HID init, and parses its HID reports. Without the init, which replays what the
//! SHIELD Tablet sends, it sends a keepalive a second and ignores every button.
//!
//! Report layouts come from the controller's HID report descriptor, as it hands it over.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ozproto::Host;
use ozproto::usb::{self, Message, Request};
use shdtray::Battery;
use tracing::{debug, info, warn};

const HID_INTERFACE: u16 = 0;
const HID_EP: u8 = 0x81;
const RETRY_AFTER: Duration = Duration::from_secs(2);
const MAX_TRIES: u32 = 3;

/// What the tablet sends after enumeration, in order. Without the first eight the
/// controller connects but never reports input, and resets after ~45 s.
/// The audio interfaces come after, see `Controller::set_streams`.
const SETUP: [&str; 14] = [
    "SET_CONFIGURATION 1",
    "SET_IDLE",
    "GET_REPORT input 0x01",
    "GET_REPORT input 0x02",
    "GET_REPORT input 0xFD",
    "GET_REPORT feature 0xFC",
    "SET_REPORT feature 0xFD=0A",
    "GET_REPORT feature 0xFD (MAC)",
    "SET_REPORT feature 0xFD=01",
    "GET_REPORT feature 0xFD (status)",
    "SET_REPORT feature 0xFD=08",
    "GET_REPORT feature 0xFD (version)",
    "SET_REPORT feature 0xFD=0D",
    "GET_REPORT feature 0xFD (handshake)",
];
/// Audio streaming interfaces, alt 1 streaming and alt 0 idle
const HEADSET_INTERFACE: u8 = 2;
const MIC_INTERFACE: u8 = 3;
/// Isoc OUT endpoint number of the headset: 32 kHz stereo s16le
pub const HEADSET_EP: u8 = 0x02;
/// Isoc IN endpoint of the mic: 16 kHz mono s16le
const MIC_EP: u8 = 0x83;

/// Feature 0xFD is a mailbox: SET writes a command byte, GET reads back the command and its
/// answer. The tablet sends 0A, 01, 08 and 0D on every connect in its handshake capture. 0D
/// answers with just 0D, a handshake check as far as anyone can tell.
const MAILBOX: u8 = 0xFD;
/// Command whose answer is the best battery candidate: `01 32 d7 xx` in every tablet capture
const STATUS_CMD: u8 = 0x01;
/// How often to re-ask STATUS_CMD while connected, so the charge pulse and tray follow the
/// charger closely. It's two small control requests; one in flight at a time.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// Battery reading below which the controller is as good as dead: at ~0xCCFD on 2026-10-08 it
/// couldn't keep its radio up even without rumble
const CRITICAL_BELOW: u16 = 0xCD40;
/// Critical for this long mid-session before acting on it, so a sag under load doesn't count.
/// At connect the controller is idle, so its first reading counts right away.
const CRITICAL_FOR: Duration = Duration::from_secs(60);
/// Rumble that says charging has begun: one pulse of the right motor, the lighter one
const CHARGE_PULSE: u16 = 0x8000;
const CHARGE_PULSE_FOR: Duration = Duration::from_millis(200);
/// Identify from the tray: two short buzzes of both motors at 80%; half was too weak to notice
const IDENTIFY_LEVEL: u16 = 0xCCCC;
const IDENTIFY: [([u16; 3], Duration); 4] = [
    ([IDENTIFY_LEVEL, 0, IDENTIFY_LEVEL], Duration::from_millis(150)),
    ([0; 3], Duration::from_millis(120)),
    ([IDENTIFY_LEVEL, 0, IDENTIFY_LEVEL], Duration::from_millis(150)),
    ([0; 3], Duration::ZERO),
];
/// STATUS_CMD answer byte 1: set while on the charger. The low 7 bits are a level band.
const STATUS_CHARGING: u8 = 0x80;
/// Battery reading, STATUS_CMD bytes 2-3 big-endian, below which rumble is cut. Near empty
/// (~0xCCFD) a rumble burst sags the cell far enough to drop the link; this is where the
/// 10 band starts, ~2 of the controller's 0x200 steps above that.
const RUMBLE_OFF_BELOW: u16 = 0xD100;
/// ...and above which it comes back. The gap rides out the sag under load.
const RUMBLE_ON_ABOVE: u16 = 0xD500;

fn setup_request(step: usize, req_id: u8) -> Request {
    let set = |cmd| Request::hid_set_report(req_id, HID_INTERFACE, usb::REPORT_FEATURE, vec![MAILBOX, cmd]);
    match step {
        0 => Request::SetConfig { req_id, config: 1 },
        1 => Request::hid_set_idle(req_id, HID_INTERFACE),
        2 => Request::hid_get_report(req_id, HID_INTERFACE, usb::REPORT_INPUT, 0x01),
        3 => Request::hid_get_report(req_id, HID_INTERFACE, usb::REPORT_INPUT, 0x02),
        4 => Request::hid_get_report(req_id, HID_INTERFACE, usb::REPORT_INPUT, 0xFD),
        5 => Request::hid_get_report(req_id, HID_INTERFACE, usb::REPORT_FEATURE, 0xFC),
        6 => set(0x0A),
        8 => set(STATUS_CMD),
        10 => set(0x08),
        12 => set(0x0D),
        _ => Request::hid_get_report(req_id, HID_INTERFACE, usb::REPORT_FEATURE, MAILBOX),
    }
}

/// Drops trailing zero padding from a 64-byte mailbox answer, keeping at least `min` bytes
fn trim_zeros(data: &[u8], min: usize) -> &[u8] {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..end.max(min).min(data.len())]
}

//region Reports

/// Report 0x01 button bits, in descriptor order
pub mod btn {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const X: u16 = 1 << 2;
    pub const Y: u16 = 1 << 3;
    pub const LB: u16 = 1 << 4;
    pub const RB: u16 = 1 << 5;
    pub const L3: u16 = 1 << 6;
    pub const R3: u16 = 1 << 7;
    /// HID button 9, BTN_TL2 on Linux. Held for as long as a volume key or the trackpad click
    /// is, rising up to ~60 ms before it; no repeat pulses. Likely a shared switch line.
    #[cfg_attr(not(test), expect(dead_code, reason = "named so the mapping can ignore it on purpose"))]
    pub const SPURIOUS: u16 = 1 << 8;
    pub const START: u16 = 1 << 9;
}

/// Report 0x01 consumer-key bits
pub mod key {
    pub const MUTE: u8 = 1 << 0;
    pub const VOL_UP: u8 = 1 << 1;
    pub const VOL_DOWN: u8 = 1 << 2;
    /// The NVIDIA button, reported as Power. Left to the controller for connecting and power.
    #[cfg_attr(not(test), expect(dead_code, reason = "named so the mapping can ignore it on purpose"))]
    pub const NVIDIA: u8 = 1 << 3;
    pub const BACK: u8 = 1 << 4;
    pub const HOME: u8 = 1 << 5;
}

/// Gamepad state from report 0x01.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u16,
    pub keys: u8,
    /// 0-7 clockwise from up, anything else = centered
    pub hat: u8,
    /// Sticks, 0-65535, centered at 0x7FFF. Y grows downward.
    pub lx: u16,
    pub ly: u16,
    pub rx: u16,
    pub ry: u16,
    /// Left and right trigger, from the Brake and Accelerator usages
    pub lt: u16,
    pub rt: u16,
}

impl Default for Pad {
    fn default() -> Self {
        Pad { buttons: 0, keys: 0, hat: 0x0F, lx: 0x7FFF, ly: 0x7FFF, rx: 0x7FFF, ry: 0x7FFF, lt: 0, rt: 0 }
    }
}

/// Trackpad state from report 0x02.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trackpad {
    /// Bit 0 = pad click, 1 = right, 2 = middle
    pub buttons: u8,
    pub touch: bool,
    /// Absolute finger position. The descriptor says relative int8; it isn't.
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    Pad(Pad),
    Trackpad(Trackpad),
}

impl Report {
    /// Parses one interrupt-IN unit, report ID included.
    pub fn parse(unit: &[u8]) -> Option<Report> {
        let le = |i: usize| u16::from_le_bytes([unit[i], unit[i + 1]]);
        match unit.first()? {
            0x01 if unit.len() >= 16 => {
                let bits = le(1);
                Some(Report::Pad(Pad {
                    buttons: bits & 0x03FF,
                    keys: (bits >> 10) as u8 & 0x3F,
                    hat: unit[3] & 0x0F,
                    lx: le(4),
                    ly: le(6),
                    rx: le(8),
                    ry: le(10),
                    lt: le(12),
                    rt: le(14),
                }))
            }
            0x02 if unit.len() >= 6 => Some(Report::Trackpad(Trackpad {
                buttons: unit[1] & 0x07,
                touch: unit[1] & 0x08 != 0,
                x: le(2),
                y: le(4),
            })),
            _ => None,
        }
    }
}

//endregion

pub struct Controller {
    step: usize,
    last_req_id: u8,
    /// Request in flight: ID, when it went out, how many times
    pending: Option<(u8, Instant, u32)>,
    ready: bool,
    /// Output report in flight: request ID, when it went out
    output_pending: Option<(u8, Instant)>,
    /// Status query in flight: request ID, when it went out, and whether it's the GET half
    status_pending: Option<(u8, Instant, bool)>,
    status_due: Option<Instant>,
    /// Last STATUS_CMD answer, to log only changes
    status: Vec<u8>,
    /// Rumble cut for low battery, see RUMBLE_OFF_BELOW
    rumble_cut: bool,
    /// Rumble cut just now; stop the motors at the next chance
    stop_motors: bool,
    /// Charger state from the last status, None before the first one this session
    charging: Option<bool>,
    /// Level band from the last status, to log changes
    band: Option<u8>,
    /// Voltage reading from the last status
    reading: Option<u16>,
    /// Since when every reading has been below CRITICAL_BELOW
    critical_since: Option<Instant>,
    /// The session's first reading was critical
    critical_at_connect: bool,
    /// Kicked for its battery; nothing more to do this session
    let_go: bool,
    /// Rumble the host plays itself, charge pulse and identify: motor values and how long to
    /// hold them, with the step running now already popped
    buzz: VecDeque<([u16; 3], Duration)>,
    buzz_until: Option<Instant>,
    /// Audio streams wanted open, mic and headset; kept across sessions
    streams_want: [bool; 2],
    /// ...and open this session
    streams_open: [bool; 2],
    /// SET_INTERFACE in flight: ID, when it went out, which stream, opening or closing
    stream_pending: Option<(u8, Instant, usize, bool)>,
    /// Mic samples since the last take_mic
    mic: Vec<u8>,
}

impl Controller {
    /// `mic` opens the mic stream after setup. A streaming controller never dozes off: without
    /// it, it idles between inputs and goes silent for up to ~2 s at a time.
    pub fn new(mic: bool) -> Self {
        Controller {
            step: 0,
            last_req_id: 0,
            pending: None,
            ready: false,
            output_pending: None,
            status_pending: None,
            status_due: None,
            status: Vec::new(),
            rumble_cut: false,
            stop_motors: false,
            charging: None,
            band: None,
            reading: None,
            critical_since: None,
            critical_at_connect: false,
            let_go: false,
            buzz: VecDeque::new(),
            buzz_until: None,
            streams_want: [mic, false],
            streams_open: [false; 2],
            stream_pending: None,
            mic: Vec::new(),
        }
    }

    /// Which audio streams to have open, now or once set up
    pub fn set_streams(&mut self, mic: bool, headset: bool) {
        self.streams_want = [mic, headset];
    }

    /// The headset stream is open
    pub fn headset_open(&self) -> bool {
        self.ready && self.streams_open[1]
    }

    /// Mic samples, 16 kHz mono s16le, since the last call
    pub fn take_mic(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.mic)
    }

    /// Two short buzzes, unless something else is buzzing
    pub fn identify(&mut self) {
        self.play(&IDENTIFY);
    }

    fn play(&mut self, pattern: &[([u16; 3], Duration)]) {
        if self.buzz.is_empty() && self.buzz_until.is_none() {
            self.buzz.extend(pattern.iter().copied());
        }
    }

    /// The last status answer, once setup is done
    pub fn battery(&self) -> Option<Battery> {
        let (Some(charging), Some(band), Some(reading)) = (self.charging, self.band, self.reading) else {
            return None;
        };
        self.ready.then_some(Battery { charging, band, reading, critical: self.rumble_cut, must_charge: self.let_go })
    }

    /// Too flat to use and not charging: critical at connect, or for CRITICAL_FOR since.
    /// See CRITICAL_BELOW.
    pub fn must_charge(&self, now: Instant) -> bool {
        !self.let_go
            && (self.critical_at_connect || self.critical_since.is_some_and(|t| now.duration_since(t) >= CRITICAL_FOR))
    }

    /// The controller got kicked for must_charge
    pub fn let_go(&mut self) {
        self.let_go = true;
    }

    /// True once the setup sequence has run; input reports follow shortly.
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// Starts the setup sequence on a fresh session.
    pub fn start(&mut self, now: Instant, host: &mut Host) {
        *self = Controller { streams_want: self.streams_want, last_req_id: self.last_req_id, ..Controller::new(false) };
        self.issue(now, host, 1);
    }

    pub fn poll(&mut self, now: Instant, host: &mut Host) {
        self.poll_status(now, host);
        self.poll_streams(now, host);
        if self.buzz_until.is_none_or(|t| now >= t) {
            match self.buzz.front() {
                Some(&(v, d)) => {
                    if self.set_output(now, host, v) {
                        self.buzz.pop_front();
                        self.buzz_until = Some(now + d);
                    }
                }
                None => self.buzz_until = None,
            }
        }
        if self.stop_motors && self.set_output(now, host, [0; 3]) {
            self.stop_motors = false;
        }
        let Some((id, at, tries)) = self.pending else {
            return;
        };
        if now.duration_since(at) < RETRY_AFTER {
            return;
        }
        if tries >= MAX_TRIES {
            warn!(req = SETUP[self.step], req_id = id, "no response after {MAX_TRIES} tries, skipping");
            self.advance(now, host);
        } else {
            debug!(req = SETUP[self.step], req_id = id, "no response, retrying");
            self.issue(now, host, tries + 1);
        }
    }

    /// Handles one USB message; returns any input reports it carried.
    pub fn on_usb(&mut self, now: Instant, msg: &Message, host: &mut Host) -> Vec<Report> {
        match msg {
            Message::EndpointData(d) if d.endpoint == HID_EP => {
                let Some(units) = d.units() else {
                    debug!(format = d.format, "HID data in an unhandled format");
                    return Vec::new();
                };
                units
                    .filter_map(|u| {
                        let r = Report::parse(u);
                        // Vendor 0xFD and anything else unmapped; looking for raw multitouch
                        if r.is_none() {
                            debug!(id = format!("{:#04x}", u.first().copied().unwrap_or(0)), len = u.len(), data = %crate::hex(u), "unmapped HID report");
                        }
                        r
                    })
                    .collect()
            }
            Message::EndpointData(d) if d.endpoint == MIC_EP => {
                // 1 ms units. Capped, since nothing may be taking them.
                if let Some(units) = d.units()
                    && self.mic.len() < 64 * 1024
                {
                    units.for_each(|u| self.mic.extend_from_slice(u));
                }
                Vec::new()
            }
            Message::EndpointData(_) => Vec::new(),
            Message::Status { req_id, rcode, .. } | Message::VendorClass { req_id, rcode, .. } => {
                self.on_response(now, *req_id, *rcode, msg, host);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// SET_REPORT output 0x01: the three u16 the descriptor puts on the gamepad report, likely
    /// rumble. False until setup is done or while the last one is unanswered.
    pub fn set_output(&mut self, now: Instant, host: &mut Host, values: [u16; 3]) -> bool {
        if let Some((id, at)) = self.output_pending
            && now.duration_since(at) >= RETRY_AFTER
        {
            warn!(req_id = id, "output report 0x01 never answered");
            self.output_pending = None;
        }
        if !self.ready || self.output_pending.is_some() {
            return false;
        }
        let values = if self.rumble_cut { [0; 3] } else { values };
        let mut data = vec![0x01];
        for v in values {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let id = self.next_req_id();
        let sent = host.submit(&Request::hid_set_report(id, HID_INTERFACE, usb::REPORT_OUTPUT, data));
        if sent {
            self.output_pending = Some((id, now));
        }
        sent
    }

    /// Re-asks STATUS_CMD every STATUS_EVERY once setup is done: SET, then GET on its answer
    fn poll_status(&mut self, now: Instant, host: &mut Host) {
        if let Some((_, at, _)) = self.status_pending
            && now.duration_since(at) >= RETRY_AFTER
        {
            debug!("status query unanswered, trying again later");
            self.status_pending = None;
        }
        if !self.ready || self.status_pending.is_some() || self.status_due.is_some_and(|t| now < t) {
            return;
        }
        let id = self.next_req_id();
        let set = Request::hid_set_report(id, HID_INTERFACE, usb::REPORT_FEATURE, vec![MAILBOX, STATUS_CMD]);
        if host.submit(&set) {
            self.status_pending = Some((id, now, false));
            self.status_due = Some(now + STATUS_EVERY);
        }
    }

    fn on_status(&mut self, now: Instant, get: bool, rcode: u8, msg: &Message, host: &mut Host) {
        self.status_pending = None;
        if rcode != 0 {
            debug!(rcode, get, "status query failed");
            return;
        }
        if !get {
            let id = self.next_req_id();
            if host.submit(&Request::hid_get_report(id, HID_INTERFACE, usb::REPORT_FEATURE, MAILBOX)) {
                self.status_pending = Some((id, now, true));
            }
            return;
        }
        if let Message::VendorClass { data, .. } = msg {
            self.log_status(now, data);
        }
    }

    /// Acts on a STATUS_CMD answer: logs changes, low-battery rumble cut, critical battery and
    /// the charge pulse
    fn log_status(&mut self, now: Instant, data: &[u8]) {
        let data = trim_zeros(data, 4);
        if data != self.status.as_slice() {
            debug!(data = %crate::hex(data), "controller status (mailbox 01)");
            self.status = data.to_vec();
        }
        let &[_, flags, hi, lo, ..] = data else { return };
        let reading = u16::from_be_bytes([hi, lo]);
        let charging = flags & STATUS_CHARGING != 0;
        let band = flags & !STATUS_CHARGING;
        if self.band != Some(band) || self.charging != Some(charging) {
            info!(band, charging, reading = format!("{reading:#06x}"), "battery");
        }
        let first = self.charging.is_none();
        if self.charging == Some(false) && charging {
            self.play(&[([0, 0, CHARGE_PULSE], CHARGE_PULSE_FOR), ([0; 3], Duration::ZERO)]);
        }
        self.charging = Some(charging);
        self.band = Some(band);
        self.reading = Some(reading);
        if charging || reading >= CRITICAL_BELOW {
            self.critical_since = None;
            self.critical_at_connect = false;
        } else if self.critical_since.is_none() {
            warn!(reading = format!("{reading:#06x}"), "battery critical");
            self.critical_since = Some(now);
            self.critical_at_connect = first;
        }
        let cut = !charging && (reading < RUMBLE_OFF_BELOW || (self.rumble_cut && reading <= RUMBLE_ON_ABOVE));
        if cut != self.rumble_cut {
            if cut {
                warn!(reading = format!("{reading:#06x}"), "battery low; rumble off so it doesn't drop the link");
                self.stop_motors = true;
            } else {
                info!(reading = format!("{reading:#06x}"), charging, "rumble back on");
            }
            self.rumble_cut = cut;
        }
    }

    fn on_response(&mut self, now: Instant, req_id: u8, rcode: u8, msg: &Message, host: &mut Host) {
        if let Some((id, _, get)) = self.status_pending
            && id == req_id
        {
            self.on_status(now, get, rcode, msg, host);
            return;
        }
        if let Some((id, _, i, open)) = self.stream_pending
            && id == req_id
        {
            self.stream_pending = None;
            if rcode == 0 {
                self.streams_open[i] = open;
                info!(stream = ["mic", "headset"][i], open, "audio");
            } else {
                warn!(stream = ["mic", "headset"][i], open, rcode, "audio interface switch failed");
            }
            return;
        }
        if self.output_pending.map(|p| p.0) == Some(req_id) {
            self.output_pending = None;
            debug!(rcode, "output report 0x01 answered");
            return;
        }
        if self.pending.map(|p| p.0) != Some(req_id) {
            return; // a repeat, or an answer to a request we gave up on
        }
        let data = match msg {
            Message::VendorClass { data, .. } => crate::hex(data),
            _ => String::new(),
        };
        if rcode == 0 {
            debug!(req = SETUP[self.step], %data, "setup");
            if let Message::VendorClass { data, .. } = msg
                && SETUP[self.step].starts_with("GET_REPORT feature 0xFD")
            {
                info!(req = SETUP[self.step], data = %crate::hex(trim_zeros(data, 1)), "mailbox");
                if data.first() == Some(&STATUS_CMD) {
                    self.log_status(now, data);
                }
            }
        } else {
            warn!(req = SETUP[self.step], rcode, "setup request failed, moving on");
        }
        self.advance(now, host);
    }

    fn advance(&mut self, now: Instant, host: &mut Host) {
        self.pending = None;
        self.step += 1;
        if self.step < SETUP.len() {
            self.issue(now, host, 1);
        } else {
            self.ready = true;
            info!("controller configured; input should start within ~10 s");
        }
    }

    /// One SET_INTERFACE at a time until the open streams match the wanted ones
    fn poll_streams(&mut self, now: Instant, host: &mut Host) {
        if let Some((_, at, ..)) = self.stream_pending
            && now.duration_since(at) >= RETRY_AFTER
        {
            debug!("audio interface switch unanswered, trying again");
            self.stream_pending = None;
        }
        if !self.ready || self.stream_pending.is_some() {
            return;
        }
        let Some(i) = (0..2).find(|&i| self.streams_want[i] != self.streams_open[i]) else { return };
        let open = self.streams_want[i];
        let interface = [MIC_INTERFACE, HEADSET_INTERFACE][i];
        let id = self.next_req_id();
        if host.submit(&Request::SetInterface { req_id: id, interface, alt: u8::from(open) }) {
            self.stream_pending = Some((id, now, i, open));
        }
    }

    fn next_req_id(&mut self) -> u8 {
        self.last_req_id = match self.last_req_id.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.last_req_id
    }

    fn issue(&mut self, now: Instant, host: &mut Host, tries: u32) {
        let id = self.next_req_id();
        if host.submit(&setup_request(self.step, id)) {
            self.pending = Some((id, now, tries));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Units below are from the first live run

    #[test]
    fn pad_report() {
        let Some(Report::Pad(p)) = Report::parse(&hex("0100000ff17ea67aff7fff7f00000000")) else { panic!() };
        assert_eq!((p.buttons, p.keys, p.hat), (0, 0, 0x0F));
        assert_eq!((p.lx, p.ly, p.rx, p.ry), (0x7EF1, 0x7AA6, 0x7FFF, 0x7FFF));
        assert_eq!((p.lt, p.rt), (0, 0));
    }

    #[test]
    fn pad_bits() {
        // A + START + NVIDIA key, hat right
        let bits: u16 = btn::A | btn::START | (u16::from(key::NVIDIA) << 10);
        let mut u = vec![0x01];
        u.extend(bits.to_le_bytes());
        u.push(0x02);
        u.extend([0u8; 12]);
        let Some(Report::Pad(p)) = Report::parse(&u) else { panic!() };
        assert_eq!(p.buttons, btn::A | btn::START);
        assert_eq!(p.keys, key::NVIDIA);
        assert_eq!(p.hat, 2);
    }

    #[test]
    fn trackpad_report() {
        let Some(Report::Trackpad(t)) = Report::parse(&hex("020891003600")) else { panic!() };
        assert!(t.touch);
        assert_eq!((t.buttons, t.x, t.y), (0, 0x91, 0x36));
        let Some(Report::Trackpad(t)) = Report::parse(&hex("0200ab004d00")) else { panic!() };
        assert!(!t.touch);
    }

    #[test]
    fn short_or_unknown_units() {
        assert_eq!(Report::parse(&hex("0100")), None);
        assert_eq!(Report::parse(&hex("fd0000")), None);
        assert_eq!(Report::parse(&[]), None);
    }

    #[test]
    fn setup_matches_tablet_bytes() {
        assert_eq!(setup_request(1, 0x0a).encode(0x09), hex("310a0109070a210a00000000"));
        assert_eq!(setup_request(6, 0x1b).encode(0x1b), hex("310c011b071b2109fd030000fd0a"));
        // Mailbox 01, as at offset 8402 of a SHIELD Tablet handshake capture
        assert_eq!(setup_request(8, 0x1f).encode(0x1f), hex("310c011f071f2109fd030000fd01"));
        // The mic SET_INTERFACE from the same capture, now sent by poll_streams
        let mic = Request::SetInterface { req_id: 0x13, interface: MIC_INTERFACE, alt: 1 };
        assert_eq!(mic.encode(0x13), hex("3106011305130301"));
    }

    #[test]
    fn audio_streams_follow_the_wanted_state() {
        let mut c = Controller::new(true);
        let mut h = Host::new(ozproto::Config::default());
        c.ready = true;
        let t0 = Instant::now();
        // No session: nothing goes out
        c.poll_streams(t0, &mut h);
        assert!(c.stream_pending.is_none());
        c.stream_pending = Some((7, t0, 0, true));
        c.on_response(t0, 7, 0, &Message::Status { kind: usb::kind::SET_INTERFACE_RSP, req_id: 7, rcode: 0 }, &mut h);
        assert_eq!(c.streams_open, [true, false]);
        assert!(!c.headset_open());
        // Kept across sessions; the open ones aren't
        c.set_streams(false, true);
        c.start(t0, &mut h);
        assert_eq!((c.streams_want, c.streams_open), ([false, true], [false; 2]));
    }

    #[test]
    fn rumble_cut_on_low_battery_w_hysteresis() {
        let mut c = Controller::new(false);
        let t0 = Instant::now();
        // Readings from the 2026-10-08 drain run
        c.log_status(t0, &hex("0132de95"));
        assert!(!c.rumble_cut);
        c.log_status(t0, &hex("010ad081"));
        assert!(c.rumble_cut && c.stop_motors);
        // A rebound inside the gap doesn't bring it back
        c.log_status(t0, &hex("010ad3f0"));
        assert!(c.rumble_cut);
        // The charger does
        c.log_status(t0, &hex("018ad3f0"));
        assert!(!c.rumble_cut);
        c.log_status(t0, &hex("010ad3f0"));
        assert!(!c.rumble_cut);
        c.log_status(t0, &hex("0100ccfd"));
        assert!(c.rumble_cut);
        c.log_status(t0, &hex("0119d6a0"));
        assert!(!c.rumble_cut);
    }

    #[test]
    fn charge_pulse_only_on_plugging_in() {
        let mut c = Controller::new(false);
        let t0 = Instant::now();
        // Already charging at connect: no pulse
        c.log_status(t0, &hex("01b2e34e"));
        assert!(c.buzz.is_empty());
        c.log_status(t0, &hex("0132dd36"));
        assert!(c.buzz.is_empty());
        c.log_status(t0, &hex("01b2e374"));
        assert_eq!(c.buzz.len(), 2);
    }

    #[test]
    fn must_charge_at_connect_or_after_a_minute() {
        let t0 = Instant::now();
        // Flat at connect: right away, once
        let mut c = Controller::new(false);
        c.log_status(t0, &hex("0100ccfd"));
        assert!(c.must_charge(t0));
        c.let_go();
        assert!(!c.must_charge(t0 + CRITICAL_FOR));
        // Flat but on the charger: never
        let mut c = Controller::new(false);
        c.log_status(t0, &hex("0180ccfd"));
        assert!(!c.must_charge(t0 + CRITICAL_FOR));
        // Sagging mid-session: only after holding a minute
        let mut c = Controller::new(false);
        c.log_status(t0, &hex("010ad100"));
        c.log_status(t0, &hex("0100ccfd"));
        assert!(!c.must_charge(t0 + Duration::from_secs(30)));
        assert!(c.must_charge(t0 + CRITICAL_FOR));
        // A recovery resets it; so does the charger
        c.log_status(t0 + Duration::from_secs(70), &hex("010acdb0"));
        assert!(!c.must_charge(t0 + Duration::from_secs(200)));
        c.log_status(t0, &hex("0100ccfd"));
        c.log_status(t0, &hex("0180ccfd"));
        assert!(!c.must_charge(t0 + CRITICAL_FOR));
    }
}
