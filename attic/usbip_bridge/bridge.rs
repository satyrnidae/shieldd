//! Maps USB/IP URBs from the Windows client onto the controller's Ozmo session.
//!
//! EP0 transfers run one at a time, like ozwpan's virtual HCD. A few standard
//! requests are answered from cached state, the rest go to the controller.
//! Requests the bridge makes for itself, the descriptor prefetch and the HID
//! init sequence, share the same queue so they never interleave w/ Windows'.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ozproto::Host;
use ozproto::usb::{self, EndpointData, Message, Request};
use tracing::{debug, info, trace, warn};

use crate::usbip::{self, DeviceInfo, EPIPE, ECONNRESET, IsoPacket, Submit};

pub const BUSID: &str = "1-1";

const DEVICE_DESC: &[u8] = include_bytes!("../../reference/device_descriptor.bin");
const CONFIG_DESC: &[u8] = include_bytes!("../../reference/config_descriptor.bin");
const HID_REPORT_DESC: &[u8] = include_bytes!("../../reference/hid_report_descriptor.bin");

const HID_INTERFACE: u16 = 0;
const RETRY_AFTER: Duration = Duration::from_secs(2);
const MAX_TRIES: u32 = 3;
/// Reports kept while Windows has no interrupt URB waiting
const MAX_BUFFERED_REPORTS: usize = 32;
/// ~250 ms of mic audio at 1 unit per ms
const MAX_BUFFERED_ISO: usize = 256;

/// What the tablet sends after SET_IDLE. W/o it the controller
/// enumerates but never reports input.
const INIT: [(&str, InitReq); 6] = [
    ("GET_REPORT input 0x01", InitReq::Get(usb::REPORT_INPUT, 0x01)),
    ("GET_REPORT input 0x02", InitReq::Get(usb::REPORT_INPUT, 0x02)),
    ("GET_REPORT input 0xFD", InitReq::Get(usb::REPORT_INPUT, 0xFD)),
    ("GET_REPORT feature 0xFC", InitReq::Get(usb::REPORT_FEATURE, 0xFC)),
    ("SET_REPORT feature 0xFD=0A", InitReq::Set(usb::REPORT_FEATURE, &[0xFD, 0x0A])),
    ("GET_REPORT feature 0xFD", InitReq::Get(usb::REPORT_FEATURE, 0xFD)),
];

#[derive(Debug, Clone, Copy)]
enum InitReq {
    Get(u8, u8),
    Set(u8, &'static [u8]),
}

//region Setup packets

const REQ_GET_STATUS: u8 = 0x00;
const REQ_CLEAR_FEATURE: u8 = 0x01;
const REQ_SET_FEATURE: u8 = 0x03;
const REQ_SET_ADDRESS: u8 = 0x05;
const REQ_GET_DESCRIPTOR: u8 = 0x06;
const REQ_GET_CONFIGURATION: u8 = 0x08;
const REQ_SET_CONFIGURATION: u8 = 0x09;
const REQ_GET_INTERFACE: u8 = 0x0A;
const REQ_SET_INTERFACE: u8 = 0x0B;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Setup {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

impl Setup {
    fn parse(b: [u8; 8]) -> Self {
        Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    fn get_descriptor(request_type: u8, desc_type: u8, index: u8, w_index: u16, length: u16) -> Self {
        Setup {
            request_type,
            request: REQ_GET_DESCRIPTOR,
            value: u16::from_le_bytes([index, desc_type]),
            index: w_index,
            length,
        }
    }

    fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }

    fn is_standard(&self) -> bool {
        self.request_type & 0x60 == 0
    }

    fn desc_type(&self) -> u8 {
        (self.value >> 8) as u8
    }
}

//endregion

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Client(u32),
    Prefetch,
    Init(usize),
}

#[derive(Debug)]
struct Control {
    origin: Origin,
    setup: Setup,
    /// OUT data stage
    out: Vec<u8>,
}

#[derive(Debug)]
struct Inflight {
    ctl: Control,
    req_id: u8,
    sent_at: Instant,
    tries: u32,
    /// Descriptor bytes collected so far
    buf: Vec<u8>,
    total: Option<u16>,
    /// Client unlinked it; finish quietly
    cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpKind {
    Isoc,
    Bulk,
    Interrupt,
}

#[derive(Debug)]
struct PendingUrb {
    seqnum: u32,
    length: u32,
    iso: Vec<IsoPacket>,
    queued_at: Instant,
}

#[derive(Debug, Default)]
struct InEndpoint {
    urbs: VecDeque<PendingUrb>,
    /// Interrupt reports or isoc units w/ no URB to go into yet
    buffered: VecDeque<Vec<u8>>,
}

#[derive(Debug, Default)]
struct Cache {
    device: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
    hid_report: Option<Vec<u8>>,
}

pub struct Bridge {
    started: Instant,
    info: Option<DeviceInfo>,
    cache: Cache,
    endpoints: HashMap<u8, EpKind>,
    ep0: VecDeque<Control>,
    inflight: Option<Inflight>,
    last_req_id: u8,
    config_value: u8,
    alts: [u8; 8],
    in_eps: HashMap<u8, InEndpoint>,
    /// Isoc OUT completions, due times in order
    iso_out: VecDeque<(Instant, u32, Vec<u8>)>,
    iso_out_next: Instant,
    iso_out_warned: bool,
    attached: bool,
    replies: Vec<Vec<u8>>,
}

impl Bridge {
    pub fn new() -> Self {
        let now = Instant::now();
        Bridge {
            started: now,
            info: None,
            cache: Cache::default(),
            endpoints: HashMap::new(),
            ep0: VecDeque::new(),
            inflight: None,
            last_req_id: 0,
            config_value: 0,
            alts: [0; 8],
            in_eps: HashMap::new(),
            iso_out: VecDeque::new(),
            iso_out_next: now,
            iso_out_warned: false,
            attached: false,
            replies: Vec::new(),
        }
    }

    /// The device as exported over USB/IP, once its descriptors are in
    pub fn device(&self) -> Option<&DeviceInfo> {
        self.info.as_ref()
    }

    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.replies)
    }

    //region Session and client lifecycle

    /// A fresh Ozmo session: the controller is unconfigured again, so start
    /// over and fetch what USB/IP needs to describe it.
    pub fn on_session_start(&mut self, now: Instant, host: &mut Host) {
        let last_req_id = self.last_req_id;
        *self = Bridge { last_req_id, ..Bridge::new() };
        // Configure it ourselves too. usbip-win2 keeps the Windows device node
        // alive across reconnects and just re-imports, so Windows may never
        // send SET_CONFIGURATION to a new session; w/o it and the HID init the
        // controller never reports.
        let set_config = Setup { request_type: 0x00, request: REQ_SET_CONFIGURATION, value: 1, index: 0, length: 0 };
        // Step 1 of the HID init. On Linux usbhid sends it; here nobody else will.
        let set_idle = Setup { request_type: usb::RT_OUT_CLASS_INTERFACE, request: usb::HID_SET_IDLE, value: 0, index: HID_INTERFACE, length: 0 };
        for setup in [
            Setup::get_descriptor(usb::RT_IN_STD_DEVICE, usb::DESC_DEVICE, 0, 0, 18),
            Setup::get_descriptor(usb::RT_IN_STD_DEVICE, usb::DESC_CONFIG, 0, 0, 0xFFFF),
            set_config,
            set_idle,
            Setup::get_descriptor(usb::RT_IN_STD_INTERFACE, usb::DESC_HID_REPORT, 0, HID_INTERFACE, 0xFFFF),
        ] {
            self.ep0.push_back(Control { origin: Origin::Prefetch, setup, out: Vec::new() });
        }
        for i in 0..INIT.len() {
            self.ep0.push_back(init_control(i));
        }
        self.pump(now, host);
    }

    pub fn on_session_end(&mut self) {
        *self = Bridge { last_req_id: self.last_req_id, ..Bridge::new() };
    }

    /// The client imported the device. Stale input from before doesn't belong to it.
    pub fn on_attach(&mut self) {
        self.attached = true;
        for ep in self.in_eps.values_mut() {
            ep.buffered.clear();
        }
    }

    /// The client went away: forget everything it had queued.
    pub fn on_detach(&mut self) {
        self.attached = false;
        self.ep0.retain(|c| !matches!(c.origin, Origin::Client(_)));
        if let Some(f) = self.inflight.as_mut()
            && matches!(f.ctl.origin, Origin::Client(_))
        {
            f.cancelled = true;
        }
        for ep in self.in_eps.values_mut() {
            ep.urbs.clear();
        }
        self.iso_out.clear();
        self.replies.clear();
    }

    //endregion

    //region Client requests

    pub fn on_submit(&mut self, now: Instant, s: Submit, host: &mut Host) {
        if s.ep == 0 {
            let setup = Setup::parse(s.setup);
            trace!(seqnum = s.seqnum, ?setup, "control URB");
            self.ep0.push_back(Control { origin: Origin::Client(s.seqnum), setup, out: s.data });
            self.pump(now, host);
            return;
        }

        let addr = s.ep | if s.direction == usbip::DIR_IN { 0x80 } else { 0 };
        trace!(seqnum = s.seqnum, ep = format!("{addr:#04x}"), len = s.transfer_buffer_length, packets = s.iso.len(), "URB");
        match (self.endpoints.get(&addr).copied(), s.direction) {
            (Some(EpKind::Interrupt | EpKind::Bulk), usbip::DIR_IN) => {
                let ep = self.in_eps.entry(addr).or_default();
                if let Some(report) = ep.buffered.pop_front() {
                    let n = report.len().min(s.transfer_buffer_length as usize);
                    self.replies.push(usbip::ret_submit(s.seqnum, 0, &report[..n], n as u32, 0, &[]));
                } else {
                    ep.urbs.push_back(PendingUrb { seqnum: s.seqnum, length: s.transfer_buffer_length, iso: Vec::new(), queued_at: now });
                }
            }
            (Some(EpKind::Isoc), usbip::DIR_IN) => {
                self.in_eps.entry(addr).or_default().urbs.push_back(PendingUrb {
                    seqnum: s.seqnum,
                    length: s.transfer_buffer_length,
                    iso: s.iso,
                    queued_at: now,
                });
            }
            (Some(EpKind::Isoc), _) => {
                // Headset audio. Paced to real time so Windows doesn't spin,
                // but not sent to the controller yet.
                if !self.iso_out_warned {
                    warn!(ep = format!("{addr:#04x}"), "isoc OUT not implemented yet; headset audio is dropped");
                    self.iso_out_warned = true;
                }
                let ms = s.iso.len().max(1) as u64;
                let due = self.iso_out_next.max(now) + Duration::from_millis(ms);
                self.iso_out_next = due;
                let iso: Vec<IsoPacket> = s.iso.iter().map(|p| IsoPacket { actual_length: p.length, status: 0, ..*p }).collect();
                let reply = usbip::ret_submit(s.seqnum, 0, &[], s.transfer_buffer_length, self.frame(now), &iso);
                self.iso_out.push_back((due, s.seqnum, reply));
            }
            _ => {
                debug!(ep = format!("{addr:#04x}"), "URB for an endpoint the device doesn't have");
                self.replies.push(usbip::ret_submit(s.seqnum, -EPIPE, &[], 0, 0, &[]));
            }
        }
    }

    pub fn on_unlink(&mut self, seqnum: u32, target: u32) {
        let found = 'find: {
            if let Some(i) = self.ep0.iter().position(|c| c.origin == Origin::Client(target)) {
                self.ep0.remove(i);
                break 'find true;
            }
            if let Some(f) = self.inflight.as_mut()
                && f.ctl.origin == Origin::Client(target)
            {
                f.cancelled = true;
                break 'find true;
            }
            for ep in self.in_eps.values_mut() {
                if let Some(i) = ep.urbs.iter().position(|u| u.seqnum == target) {
                    ep.urbs.remove(i);
                    break 'find true;
                }
            }
            if let Some(i) = self.iso_out.iter().position(|(_, s, _)| *s == target) {
                self.iso_out.remove(i);
                break 'find true;
            }
            false
        };
        // Found: the URB dies w/o a RET_SUBMIT. Not found: it already completed.
        self.replies.push(usbip::ret_unlink(seqnum, if found { -ECONNRESET } else { 0 }));
    }

    //endregion

    //region Controller traffic

    pub fn on_usb(&mut self, now: Instant, msg: &Message, host: &mut Host) {
        if let Message::EndpointData(d) = msg {
            self.on_endpoint_data(now, d);
            return;
        }
        let (req_id, rcode) = match msg {
            Message::GetDesc { req_id, rcode, .. }
            | Message::Status { req_id, rcode, .. }
            | Message::VendorClass { req_id, rcode, .. } => (*req_id, *rcode),
            _ => return,
        };
        let Some(f) = self.inflight.as_mut().filter(|f| f.req_id == req_id) else {
            trace!(req_id, "response to no request in flight");
            return;
        };

        if rcode != 0 {
            debug!(setup = ?f.ctl.setup, rcode, "controller rejected request");
            let f = self.inflight.take().unwrap();
            self.complete(now, f, Err(()), host);
            return;
        }

        match msg {
            Message::GetDesc { offset, total_size, data, .. } => {
                if usize::from(*offset) != f.buf.len() {
                    // A repeat of a chunk we already have. Asking for the next
                    // chunk again here is what multiplied requests before.
                    trace!(req_id, offset, have = f.buf.len(), "duplicate descriptor chunk");
                    return;
                }
                f.buf.extend_from_slice(data);
                f.total = Some(*total_size);
                let want = usize::from(f.ctl.setup.length.min(*total_size));
                if f.buf.len() < want && !data.is_empty() {
                    let req = desc_request(f);
                    f.sent_at = now;
                    host.submit(&req);
                    return;
                }
                f.buf.truncate(want);
                let f = self.inflight.take().unwrap();
                let data = f.buf.clone();
                self.complete(now, f, Ok(data), host);
            }
            Message::VendorClass { data, .. } => {
                let mut data = data.clone();
                data.truncate(usize::from(f.ctl.setup.length));
                let f = self.inflight.take().unwrap();
                self.complete(now, f, Ok(data), host);
            }
            _ => {
                let f = self.inflight.take().unwrap();
                self.complete(now, f, Ok(Vec::new()), host);
            }
        }
    }

    fn on_endpoint_data(&mut self, now: Instant, d: &EndpointData) {
        let Some(units) = d.units() else {
            debug!(ep = d.endpoint, format = d.format, "endpoint data in an unhandled format");
            return;
        };
        let kind = self.endpoints.get(&d.endpoint).copied();
        let frame = self.frame(now);
        let ep = self.in_eps.entry(d.endpoint).or_default();
        for unit in units {
            match kind {
                Some(EpKind::Interrupt | EpKind::Bulk) => {
                    trace!(ep = d.endpoint, data = %crate::hex(unit), "report");
                    if let Some(u) = ep.urbs.pop_front() {
                        let n = unit.len().min(u.length as usize);
                        self.replies.push(usbip::ret_submit(u.seqnum, 0, &unit[..n], n as u32, frame, &[]));
                    } else {
                        if ep.buffered.len() == MAX_BUFFERED_REPORTS {
                            ep.buffered.pop_front();
                        }
                        ep.buffered.push_back(unit.to_vec());
                    }
                }
                Some(EpKind::Isoc) => {
                    if ep.buffered.len() == MAX_BUFFERED_ISO {
                        ep.buffered.pop_front();
                    }
                    ep.buffered.push_back(unit.to_vec());
                }
                None => {}
            }
        }
    }

    //endregion

    /// Timeouts and isoc pacing. Call every few ms.
    pub fn poll(&mut self, now: Instant, host: &mut Host) {
        if let Some(f) = self.inflight.as_mut()
            && now.duration_since(f.sent_at) > RETRY_AFTER
        {
            if f.tries >= MAX_TRIES {
                warn!(setup = ?f.ctl.setup, "no response after {MAX_TRIES} tries, failing it");
                let f = self.inflight.take().unwrap();
                self.complete(now, f, Err(()), host);
            } else {
                warn!(setup = ?f.ctl.setup, try_ = f.tries + 1, "no response, retrying");
                f.tries += 1;
                f.sent_at = now;
                let req = ozmo_request(f);
                host.submit(&req);
            }
        }

        while self.iso_out.front().is_some_and(|(due, _, _)| *due <= now) {
            let (_, _, reply) = self.iso_out.pop_front().unwrap();
            self.replies.push(reply);
        }

        let frame = self.frame(now);
        for (addr, ep) in &mut self.in_eps {
            if self.endpoints.get(addr) != Some(&EpKind::Isoc) {
                continue;
            }
            while let Some(u) = ep.urbs.front() {
                let n = u.iso.len().max(1);
                let waited = now.duration_since(u.queued_at);
                if ep.buffered.len() < n && waited < Duration::from_millis(n as u64) {
                    break;
                }
                let u = ep.urbs.pop_front().unwrap();
                let mut data = Vec::new();
                let iso: Vec<IsoPacket> = u
                    .iso
                    .iter()
                    .map(|p| {
                        let unit = ep.buffered.pop_front().unwrap_or_default();
                        let take = unit.len().min(p.length as usize);
                        data.extend_from_slice(&unit[..take]);
                        IsoPacket { actual_length: take as u32, status: 0, ..*p }
                    })
                    .collect();
                self.replies.push(usbip::ret_submit(u.seqnum, 0, &data, data.len() as u32, frame, &iso));
            }
        }
    }

    //region EP0 machinery

    fn pump(&mut self, now: Instant, host: &mut Host) {
        while self.inflight.is_none() {
            let Some(ctl) = self.ep0.pop_front() else { return };
            if let Origin::Client(seqnum) = ctl.origin
                && let Some(result) = self.answer_locally(&ctl.setup)
            {
                trace!(seqnum, setup = ?ctl.setup, "answered locally");
                self.reply_control(seqnum, &ctl.setup, result);
                continue;
            }
            self.last_req_id = match self.last_req_id.wrapping_add(1) {
                0 => 1,
                n => n,
            };
            let f = Inflight {
                ctl,
                req_id: self.last_req_id,
                sent_at: now,
                tries: 1,
                buf: Vec::new(),
                total: None,
                cancelled: false,
            };
            match f.ctl.setup.is_standard().then_some(f.ctl.setup.request) {
                Some(REQ_GET_DESCRIPTOR | REQ_SET_CONFIGURATION | REQ_SET_INTERFACE | REQ_SET_FEATURE | REQ_CLEAR_FEATURE)
                | None => {}
                Some(_) => {
                    // A standard request Ozmo has no message for
                    self.complete(now, f, Err(()), host);
                    continue;
                }
            }
            let req = ozmo_request(&f);
            if host.submit(&req) {
                self.inflight = Some(f);
            } else {
                self.complete(now, f, Err(()), host);
            }
        }
    }

    /// Standard requests ozwpan short-circuits, plus descriptors we already have.
    fn answer_locally(&self, s: &Setup) -> Option<Result<Vec<u8>, ()>> {
        if !s.is_standard() {
            return None;
        }
        let cached = |d: &Option<Vec<u8>>| d.as_ref().map(|d| Ok(d.clone()));
        match s.request {
            REQ_GET_DESCRIPTOR => match (s.request_type, s.desc_type()) {
                (usb::RT_IN_STD_DEVICE, usb::DESC_DEVICE) => cached(&self.cache.device),
                (usb::RT_IN_STD_DEVICE, usb::DESC_CONFIG) if s.value & 0xFF == 0 => cached(&self.cache.config),
                (usb::RT_IN_STD_INTERFACE, usb::DESC_HID_REPORT) if s.index == HID_INTERFACE => cached(&self.cache.hid_report),
                _ => None,
            },
            // self-powered, per the config descriptor
            REQ_GET_STATUS => Some(Ok(if s.request_type & 0x1F == 0 { vec![1, 0] } else { vec![0, 0] })),
            REQ_GET_CONFIGURATION => Some(Ok(vec![self.config_value])),
            // Already in that config: forwarding would reset the controller's
            // state and need the HID init again for nothing
            REQ_SET_CONFIGURATION if s.value as u8 == self.config_value && self.config_value != 0 => Some(Ok(Vec::new())),
            REQ_GET_INTERFACE => Some(Ok(vec![*self.alts.get(usize::from(s.index)).unwrap_or(&0)])),
            REQ_SET_ADDRESS => Some(Ok(Vec::new())),
            _ => None,
        }
    }

    fn complete(&mut self, now: Instant, f: Inflight, result: Result<Vec<u8>, ()>, host: &mut Host) {
        let s = f.ctl.setup;
        if result.is_ok() && s.is_standard() {
            match s.request {
                REQ_SET_CONFIGURATION => {
                    self.config_value = s.value as u8;
                    if let Some(i) = self.info.as_mut() {
                        i.configuration_value = self.config_value;
                    }
                    info!(config = self.config_value, origin = ?f.ctl.origin, "configured");
                    // Our own SET_CONFIGURATION already has the init queued behind it.
                    // A different config from Windows resets the device, so replay it.
                    if self.config_value != 0 && matches!(f.ctl.origin, Origin::Client(_)) {
                        for i in 0..INIT.len() {
                            self.ep0.push_back(init_control(i));
                        }
                    }
                }
                REQ_SET_INTERFACE => {
                    if let Some(a) = self.alts.get_mut(usize::from(s.index)) {
                        *a = s.value as u8;
                    }
                    debug!(interface = s.index, alt = s.value, "interface alt set");
                }
                _ => {}
            }
        }

        match f.ctl.origin {
            Origin::Client(seqnum) if !f.cancelled => self.reply_control(seqnum, &s, result),
            Origin::Client(_) => {}
            Origin::Prefetch => self.prefetched(&s, result),
            Origin::Init(i) => match result {
                Ok(data) => {
                    debug!(req = INIT[i].0, data = %crate::hex(&data), "init");
                    if i + 1 == INIT.len() {
                        info!("HID init done; input reports should start shortly");
                    }
                }
                Err(()) => warn!(req = INIT[i].0, "init request failed"),
            },
        }
        self.pump(now, host);
    }

    fn reply_control(&mut self, seqnum: u32, s: &Setup, result: Result<Vec<u8>, ()>) {
        let reply = match result {
            Ok(mut data) => {
                if !s.is_in() {
                    // OUT: report the data stage as fully sent
                    let n = u32::from(s.length);
                    usbip::ret_submit(seqnum, 0, &[], n, 0, &[])
                } else {
                    data.truncate(usize::from(s.length));
                    usbip::ret_submit(seqnum, 0, &data, data.len() as u32, 0, &[])
                }
            }
            Err(()) => usbip::ret_submit(seqnum, -EPIPE, &[], 0, 0, &[]),
        };
        self.replies.push(reply);
    }

    fn prefetched(&mut self, s: &Setup, result: Result<Vec<u8>, ()>) {
        let Ok(data) = result else {
            warn!(setup = ?s, "prefetch failed");
            return;
        };
        let reference = match s.desc_type() {
            usb::DESC_DEVICE => {
                self.cache.device = Some(data.clone());
                DEVICE_DESC
            }
            usb::DESC_CONFIG => {
                self.endpoints = parse_endpoints(&data);
                self.cache.config = Some(data.clone());
                CONFIG_DESC
            }
            usb::DESC_HID_REPORT => {
                self.cache.hid_report = Some(data.clone());
                HID_REPORT_DESC
            }
            _ => return,
        };
        if data != reference {
            warn!(desc_type = s.desc_type(), len = data.len(), "descriptor differs from reference");
        }
        if let (Some(d), Some(c), Some(_)) = (&self.cache.device, &self.cache.config, &self.cache.hid_report) {
            self.info = DeviceInfo::from_descriptors(BUSID, d, c);
            if let Some(i) = &self.info {
                info!(busid = BUSID, "controller {:04x}:{:04x} ready to attach", i.id_vendor, i.id_product);
            }
        }
    }

    fn frame(&self, now: Instant) -> i32 {
        // Full-speed frames are 1 ms
        (now.duration_since(self.started).as_millis() & 0x7FF) as i32
    }

    //endregion
}

fn init_control(i: usize) -> Control {
    let (setup, out) = match INIT[i].1 {
        InitReq::Get(kind, id) => (
            Setup { request_type: usb::RT_IN_CLASS_INTERFACE, request: usb::HID_GET_REPORT, value: u16::from_le_bytes([id, kind]), index: HID_INTERFACE, length: 64 },
            Vec::new(),
        ),
        InitReq::Set(kind, data) => (
            Setup { request_type: usb::RT_OUT_CLASS_INTERFACE, request: usb::HID_SET_REPORT, value: u16::from_le_bytes([data[0], kind]), index: HID_INTERFACE, length: data.len() as u16 },
            data.to_vec(),
        ),
    };
    Control { origin: Origin::Init(i), setup, out }
}

/// The Ozmo message for an EP0 transfer, resuming a descriptor read where it left off.
fn ozmo_request(f: &Inflight) -> Request {
    let s = &f.ctl.setup;
    let id = f.req_id;
    if !s.is_standard() {
        return Request::VendorClass { req_id: id, req_type: s.request_type, request: s.request, value: s.value, index: s.index, data: f.ctl.out.clone() };
    }
    match s.request {
        REQ_GET_DESCRIPTOR => desc_request(f),
        REQ_SET_CONFIGURATION => Request::SetConfig { req_id: id, config: s.value as u8 },
        REQ_SET_INTERFACE => Request::SetInterface { req_id: id, interface: s.index as u8, alt: s.value as u8 },
        r => Request::Feature { set: r == REQ_SET_FEATURE, req_id: id, recipient: s.request_type & 0x1F, index: s.index as u8, feature: s.value },
    }
}

fn desc_request(f: &Inflight) -> Request {
    let s = &f.ctl.setup;
    let offset = f.buf.len() as u16;
    let want = f.total.map_or(s.length, |t| t.min(s.length));
    Request::get_desc(f.req_id, s.request_type, s.desc_type(), s.value as u8, s.index, offset, want.saturating_sub(offset))
}

/// Endpoint address -> transfer type, across all interfaces and alt settings.
fn parse_endpoints(config: &[u8]) -> HashMap<u8, EpKind> {
    let mut out = HashMap::new();
    let mut off = 0;
    while off + 2 <= config.len() {
        let len = usize::from(config[off]);
        if len < 2 {
            break;
        }
        if config[off + 1] == 0x05 && len >= 7 && off + 4 <= config.len() {
            let kind = match config[off + 3] & 0x03 {
                1 => EpKind::Isoc,
                2 => EpKind::Bulk,
                3 => EpKind::Interrupt,
                _ => {
                    off += len;
                    continue;
                }
            };
            out.insert(config[off + 2], kind);
        }
        off += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_from_reference_config() {
        let eps = parse_endpoints(CONFIG_DESC);
        assert_eq!(eps.get(&0x81), Some(&EpKind::Interrupt));
        assert_eq!(eps.get(&0x02), Some(&EpKind::Isoc));
        assert_eq!(eps.get(&0x83), Some(&EpKind::Isoc));
        assert_eq!(eps.len(), 3);
    }

    #[test]
    fn init_controls_match_tablet() {
        let c = init_control(4);
        assert_eq!(c.setup.value, 0x03FD);
        assert_eq!(c.out, [0xFD, 0x0A]);
        let c = init_control(3);
        assert_eq!((c.setup.request_type, c.setup.value), (0xA1, 0x03FC));
    }
}
