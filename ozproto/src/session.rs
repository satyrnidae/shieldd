//! Host side of an Ozmo session w/ one device (a "PD" in ozwpan terms).
//!
//! Sans-I/O: feed received frames to [`Host::on_frame`], call [`Host::poll`]
//! periodically for timeouts, and send whatever [`Host::take_tx`] hands back.
//!
//! The SHIELD controller runs in triggered mode. The host never transmits on
//! its own: every device frame w/ ACK_REQUESTED is a trigger, and the host
//! answers it w/ its queued frames, or w/ one empty frame if it has nothing.
//! Frames stay queued until the device acknowledges them, and get resent on
//! every trigger until then.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::usb::{self, SeqState};
use crate::wire::{
    self, ConnectReq, ConnectRsp, Element, Elements, Header, UpdateParam, app, base_control, ctl, elt, status,
};

pub type Mac = [u8; 6];

/// Unacknowledged frames the host may have in flight (`OZ_MAX_QUEUED_FRAMES`)
const MAX_UNACKED: usize = 4;

#[derive(Debug, Clone)]
pub struct Config {
    /// Apps the host supports, as a connect bitmap. Bit 0 is the session
    /// itself. The tablet answers 0x0012: USB + serial.
    pub host_apps: u16,
    /// First session ID handed out. ozwpan starts at 1; the tablet used 2.
    pub first_session_id: u8,
}

impl Default for Config {
    fn default() -> Self {
        Config { host_apps: 1 | (1 << app::USB) | (1 << app::SERIAL), first_session_id: 1 }
    }
}

/// Something the caller should know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A fresh session started; anything queued for the old one is gone.
    Connected {
        peer: Mac,
        session_id: u8,
        req: ConnectReq,
    },
    /// The device asked to resume the current session.
    Resumed {
        session_id: u8,
    },
    /// Connect refused, w/ the status that was sent back
    Rejected {
        peer: Mac,
        status: u8,
    },
    Usb(usb::Message),
    /// Serial app payload, after app ID and sequence number
    Serial(Vec<u8>),
    Farewell {
        ep: u8,
        index: u8,
        report: Vec<u8>,
    },
    UpdateParam(UpdateParam),
    /// The device said it's going to sleep.
    Disconnect,
    /// No frames for longer than the device's presleep time
    Asleep,
    /// Asleep for longer than the keepalive; the session is gone.
    Stopped,
}

#[derive(Debug)]
struct TxFrame {
    pkt_num: u32,
    elements: Vec<u8>,
}

#[derive(Debug)]
struct Session {
    peer: Mac,
    id: u8,
    mode: u8,
    max_tx: usize,
    presleep: Duration,
    keep_alive: Option<Duration>,
    last_rx_at: Instant,
    last_rx_pkt: u32,
    asleep: bool,
    /// Packet number of the last trigger, echoed back in `last_pkt_num`
    trigger_pkt: u32,
    last_tx_pkt: u32,
    unacked: VecDeque<TxFrame>,
    /// Encoded elements waiting for room in a frame
    pending: VecDeque<Vec<u8>>,
    usb_seq: SeqState,
    serial_seq: SeqState,
}

impl Session {
    fn new(peer: Mac, id: u8, req: &ConnectReq, pkt_num: u32, now: Instant) -> Self {
        Session {
            peer,
            id,
            mode: req.mode,
            max_tx: req.max_tx_size(),
            presleep: req.presleep_time(),
            keep_alive: req.keep_alive_time(),
            last_rx_at: now,
            last_rx_pkt: pkt_num,
            asleep: false,
            trigger_pkt: pkt_num,
            last_tx_pkt: 0,
            unacked: VecDeque::new(),
            pending: VecDeque::new(),
            usb_seq: SeqState::default(),
            serial_seq: SeqState::default(),
        }
    }

    fn triggered(&self) -> bool {
        self.mode & wire::mode::MASK == wire::mode::TRIGGERED
    }

    /// Drops frames the device has acknowledged. `lpn` is the low byte of the
    /// newest packet number it has seen; anything within half a cycle behind
    /// it counts as delivered.
    fn retire(&mut self, lpn: u8) {
        while let Some(f) = self.unacked.front() {
            let behind = lpn.wrapping_sub(f.pkt_num as u8);
            if f.pkt_num == 0 || behind > 127 {
                break;
            }
            self.unacked.pop_front();
        }
    }

    /// Packs pending elements into new frames while there's room.
    fn fill(&mut self) {
        while self.unacked.len() < MAX_UNACKED && !self.pending.is_empty() {
            let mut elements = Vec::new();
            while let Some(e) = self.pending.front() {
                if !elements.is_empty() && Header::LEN + elements.len() + e.len() > self.max_tx {
                    break;
                }
                elements.extend(self.pending.pop_front().unwrap());
            }
            self.push_frame(elements);
        }
    }

    fn push_frame(&mut self, elements: Vec<u8>) {
        self.last_tx_pkt = self.last_tx_pkt.wrapping_add(1);
        self.unacked.push_back(TxFrame { pkt_num: self.last_tx_pkt, elements });
    }

    /// Answers a trigger w/ everything unacknowledged, chained w/ MORE_DATA,
    /// or w/ one empty frame. Same as ozwpan. Every trigger gets an answer:
    /// the controller dozes off when one goes unanswered, and frames sent
    /// while it dozes sit in the GO's power-save buffer until it wakes.
    /// Keeping one frame in flight instead (tried 2026-10-07) took requests
    /// from ~100 ms to ~1.5 s each.
    fn respond(&mut self, out: &mut Vec<(Mac, Vec<u8>)>) {
        self.fill();
        if self.unacked.is_empty() {
            self.push_frame(Vec::new());
        }
        let last = self.unacked.len() - 1;
        for (i, f) in self.unacked.iter().enumerate() {
            let mut control = base_control() | ctl::ACK_REQUESTED;
            if i < last {
                control |= ctl::MORE_DATA;
            }
            let mut frame = Vec::with_capacity(Header::LEN + f.elements.len());
            Header { control, last_pkt_num: self.trigger_pkt as u8, pkt_num: f.pkt_num }.write(&mut frame);
            frame.extend_from_slice(&f.elements);
            out.push((self.peer, frame));
        }
    }
}

/// Host-side protocol state for a single device.
#[derive(Debug)]
pub struct Host {
    cfg: Config,
    session: Option<Session>,
    last_session_id: u8,
    tx: Vec<(Mac, Vec<u8>)>,
}

impl Host {
    pub fn new(cfg: Config) -> Self {
        let last_session_id = cfg.first_session_id.wrapping_sub(1);
        Host { cfg, session: None, last_session_id, tx: Vec::new() }
    }

    /// The connected device, if any
    pub fn peer(&self) -> Option<Mac> {
        self.session.as_ref().map(|s| s.peer)
    }

    pub fn session_id(&self) -> Option<u8> {
        self.session.as_ref().map(|s| s.id)
    }

    /// Queues a DISCONNECT element: the device's own "going to sleep", sent the other way, as
    /// a goodbye. ozwpan never sends one; what the controller makes of it is being found out.
    /// False if there's no session.
    pub fn say_goodbye(&mut self) -> bool {
        let Some(s) = self.session.as_mut() else { return false };
        let mut e = Vec::with_capacity(2);
        wire::push_element(&mut e, elt::DISCONNECT, &[]);
        s.pending.push_back(e);
        true
    }

    /// Every element queued so far has been acknowledged; the empty frames that answer
    /// triggers don't count
    pub fn all_delivered(&self) -> bool {
        self.session.as_ref().is_none_or(|s| s.pending.is_empty() && s.unacked.iter().all(|f| f.elements.is_empty()))
    }

    /// Drops the session at once, for a device the host sent away. Stopped if there was one.
    pub fn end_session(&mut self) -> Option<Event> {
        self.session.take().map(|_| Event::Stopped)
    }

    /// Frames to put on the wire, w/ their destination.
    pub fn take_tx(&mut self) -> Vec<(Mac, Vec<u8>)> {
        std::mem::take(&mut self.tx)
    }

    /// Queues a USB request for the next trigger. False if there's no session.
    pub fn submit(&mut self, req: &usb::Request) -> bool {
        let Some(s) = self.session.as_mut() else { return false };
        let seq = s.usb_seq.next_tx();
        s.pending.push_back(req.encode(seq));
        true
    }

    /// Sends `ms` 1 ms frames of `data` to OUT endpoint `ep` (number only) as one isoc-large
    /// unit, the way ozwpan does w/ ISOC_NO_ELTS | ISOC_ANYTIME: right away, packet number 0,
    /// never retransmitted. False if there's no session.
    pub fn send_isoc(&mut self, ep: u8, frame_number: u8, ms: u8, data: &[u8]) -> bool {
        let Some(s) = self.session.as_ref() else { return false };
        let mut frame = Vec::with_capacity(Header::LEN + 4 + data.len());
        let control = base_control() | ctl::ISOC;
        Header { control, last_pkt_num: s.trigger_pkt as u8, pkt_num: 0 }.write(&mut frame);
        frame.extend_from_slice(&[ep, usb::format::ISOC_LARGE, ms, frame_number]);
        frame.extend_from_slice(data);
        self.tx.push((s.peer, frame));
        true
    }

    /// Handles one received Ozmo payload: everything after the Ethernet header.
    pub fn on_frame(&mut self, now: Instant, src: Mac, payload: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        let Some((hdr, body)) = Header::parse(payload) else { return events };
        if hdr.version() != wire::PROTOCOL_VERSION {
            return events;
        }

        let mut dup = false;
        if let Some(s) = self.session.as_mut().filter(|s| s.peer == src) {
            s.last_rx_at = now;
            s.asleep = false;
            dup = hdr.pkt_num == s.last_rx_pkt;
            s.last_rx_pkt = hdr.pkt_num;
            if !dup && s.triggered() {
                if hdr.has(ctl::ACK) {
                    s.retire(hdr.last_pkt_num);
                }
                if hdr.has(ctl::ACK_REQUESTED) {
                    // Same order as ozwpan: answer the trigger before looking at
                    // its elements, so replies to them go out on the next one.
                    s.trigger_pkt = hdr.pkt_num;
                    s.respond(&mut self.tx);
                }
            }
        }

        if hdr.has(ctl::ISOC) {
            return events; // isoc-large unit, no elements
        }
        for e in Elements::new(body) {
            self.on_element(now, src, &hdr, e, dup, &mut events);
        }
        events
    }

    fn on_element(&mut self, now: Instant, src: Mac, hdr: &Header, e: Element<'_>, dup: bool, events: &mut Vec<Event>) {
        match e.kind {
            elt::CONNECT_REQ => {
                if let Some(req) = ConnectReq::parse(e.body) {
                    self.on_connect(now, src, hdr, req, events);
                }
            }
            elt::DISCONNECT if self.peer() == Some(src) => {
                if let Some(s) = self.session.as_mut() {
                    s.asleep = true;
                }
                events.push(Event::Disconnect);
            }
            elt::UPDATE_PARAM_REQ if self.peer() == Some(src) => {
                if let (Some(p), Some(s)) = (UpdateParam::parse(e.body), self.session.as_mut()) {
                    s.presleep = wire::presleep_time(p.presleep);
                    s.keep_alive = wire::keep_alive_time(p.keep_alive);
                    events.push(Event::UpdateParam(p));
                }
            }
            elt::FAREWELL_REQ if self.peer() == Some(src) => {
                if let &[ep, index, ref report @ ..] = e.body {
                    events.push(Event::Farewell { ep, index, report: report.to_vec() });
                }
            }
            elt::APP_DATA if !dup && self.peer() == Some(src) => {
                let Some(s) = self.session.as_mut() else { return };
                match *e.body {
                    [app::USB, seq, ..] if s.usb_seq.accept_rx(seq) => {
                        if let Some((_, msg)) = usb::Message::parse(e.body) {
                            events.push(Event::Usb(msg));
                        }
                    }
                    [app::SERIAL, seq, ref rest @ ..] if s.serial_seq.accept_rx(seq) => {
                        events.push(Event::Serial(rest.to_vec()));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn on_connect(&mut self, now: Instant, src: Mac, hdr: &Header, req: ConnectReq, events: &mut Vec<Event>) {
        let apps = req.apps & self.cfg.host_apps & !1;
        let current = self.session.as_ref().filter(|s| s.peer == src).map(|s| s.id);

        let (status, session_id) = match (req.session_id, current) {
            (0, _) => {
                // Fresh start. ozwpan tears an existing session down first and
                // lets the device retry; starting over directly saves a round trip.
                let id = self.new_session_id(0);
                self.session = Some(Session::new(src, id, &req, hdr.pkt_num, now));
                events.push(Event::Connected { peer: src, session_id: id, req });
                (status::SUCCESS, id)
            }
            (asked, Some(id)) if asked == id => {
                if let Some(s) = self.session.as_mut() {
                    s.mode = req.mode;
                    s.max_tx = req.max_tx_size();
                    s.presleep = req.presleep_time();
                    s.keep_alive = req.keep_alive_time();
                    s.trigger_pkt = hdr.pkt_num;
                }
                events.push(Event::Resumed { session_id: id });
                (status::SUCCESS, id)
            }
            (asked, Some(_)) => {
                self.session = None;
                events.push(Event::Rejected { peer: src, status: status::SESSION_MISMATCH });
                (status::SESSION_MISMATCH, asked)
            }
            (asked, None) => {
                // Resuming a session we never had, e.g. after a host restart.
                // ozwpan hands out a new ID and lets the device catch up.
                let id = self.new_session_id(asked);
                self.session = Some(Session::new(src, id, &req, hdr.pkt_num, now));
                events.push(Event::Connected { peer: src, session_id: id, req });
                (status::SUCCESS, id)
            }
        };

        let rsp = if status == status::SUCCESS {
            ConnectRsp { mode: req.mode, status, session_id, apps }
        } else {
            ConnectRsp { mode: 0, status, session_id: 0, apps: 0 }
        };
        // Not part of the triggered flow: packet number 0, never retransmitted.
        // The tablet echoes the request's packet number; ozwpan sends 0.
        let mut frame = Vec::with_capacity(Header::LEN + 2 + ConnectRsp::LEN);
        Header { control: base_control(), last_pkt_num: hdr.pkt_num as u8, pkt_num: 0 }.write(&mut frame);
        wire::push_element(&mut frame, elt::CONNECT_RSP, &rsp.encode());
        self.tx.push((src, frame));
    }

    fn new_session_id(&mut self, exclude: u8) -> u8 {
        loop {
            self.last_session_id = self.last_session_id.wrapping_add(1);
            if self.last_session_id != 0 && self.last_session_id != exclude {
                return self.last_session_id;
            }
        }
    }

    /// Timeouts. Call every few hundred ms or so.
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(s) = self.session.as_mut() else { return events };
        let idle = now.saturating_duration_since(s.last_rx_at);
        if !s.asleep && idle > s.presleep {
            s.asleep = true;
            events.push(Event::Asleep);
        }
        if s.asleep {
            let limit = s.presleep + s.keep_alive.unwrap_or(Duration::ZERO);
            if idle > limit {
                self.session = None;
                events.push(Event::Stopped);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::{hex, tablet_connect_req_frame};

    const CTRL: Mac = [0x00, 0x04, 0x4b, 0x21, 0x4a, 0xf8];

    fn frame(control: u8, last: u8, pkt: u32, elements: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        Header { control, last_pkt_num: last, pkt_num: pkt }.write(&mut f);
        f.extend_from_slice(elements);
        f
    }

    fn connected() -> (Host, Instant) {
        let mut h = Host::new(Config { first_session_id: 2, ..Config::default() });
        let t = Instant::now();
        let ev = h.on_frame(t, CTRL, &tablet_connect_req_frame());
        assert!(matches!(ev[..], [Event::Connected { session_id: 2, .. }]), "{ev:?}");
        (h, t)
    }

    #[test]
    fn goodbye_goes_out_on_the_next_trigger() {
        let (mut h, t) = connected();
        h.take_tx();
        assert!(h.all_delivered());
        assert!(h.say_goodbye());
        assert!(!h.all_delivered());
        // A trigger from the device, w/ ACK_REQUESTED
        h.on_frame(t, CTRL, &frame(0x84, 0, 0x9e, &[]));
        let tx = h.take_tx();
        assert_eq!(tx[0].1[6..], hex("0800")[..]);
        // ...and its acknowledgment of packet 1
        h.on_frame(t, CTRL, &frame(0x94, 1, 0x9f, &[]));
        assert!(h.all_delivered());
    }

    #[test]
    fn isoc_large_unit() {
        let (mut h, _) = connected();
        h.take_tx();
        assert!(h.send_isoc(2, 8, 4, &[0xAA; 512]));
        let tx = h.take_tx();
        // version + ISOC, last trigger 0x9d from the connect request, pkt 0
        assert_eq!(tx[0].1[..10], hex("249d0000000002070408")[..]);
        assert_eq!(tx[0].1.len(), 6 + 4 + 512);
    }

    #[test]
    fn connect_rsp_matches_tablet() {
        let (mut h, _) = connected();
        let tx = h.take_tx();
        assert_eq!(tx.len(), 1);
        assert_eq!(tx[0].0, CTRL);
        // frame 6 of the tablet capture
        assert_eq!(tx[0].1, hex("049d00000000070cc10000000002120000000000"));
    }

    #[test]
    fn empty_trigger_exchange_matches_tablet() {
        let (mut h, t) = connected();
        h.take_tx();
        // frame 7: first trigger, nothing to ack yet
        h.on_frame(t, CTRL, &frame(0x84, 0, 7_446_942, &[]));
        assert_eq!(h.take_tx(), [(CTRL, hex("849e01000000"))]);
        // frame 9: acks pkt 1 and triggers again
        h.on_frame(t, CTRL, &frame(0x94, 1, 7_446_943, &[]));
        assert_eq!(h.take_tx(), [(CTRL, hex("849f02000000"))]);
    }

    #[test]
    fn request_goes_out_on_next_trigger_and_is_resent_until_acked() {
        let (mut h, t) = connected();
        h.take_tx();
        h.on_frame(t, CTRL, &frame(0x84, 0, 100, &[]));
        h.take_tx(); // pkt 1, empty
        assert!(h.submit(&usb::Request::get_desc(1, usb::RT_IN_STD_DEVICE, usb::DESC_DEVICE, 0, 0, 0, 8)));
        // ack 1, trigger: request goes out as pkt 2
        h.on_frame(t, CTRL, &frame(0x94, 1, 101, &[]));
        let tx = h.take_tx();
        assert_eq!(tx[0].1, hex("846502000000310d01000101000008008001000000"));
        // trigger w/o ack of 2: resent as-is
        h.on_frame(t, CTRL, &frame(0x94, 1, 102, &[]));
        let tx = h.take_tx();
        assert_eq!(tx.len(), 1);
        assert_eq!(&tx[0].1[2..6], &2u32.to_le_bytes());
        // ack 2: back to empty frames
        h.on_frame(t, CTRL, &frame(0x94, 2, 103, &[]));
        assert_eq!(h.take_tx()[0].1, hex("846703000000"));
    }

    #[test]
    fn duplicate_frames_dont_trigger() {
        let (mut h, t) = connected();
        h.take_tx();
        h.on_frame(t, CTRL, &frame(0x84, 0, 200, &[]));
        assert_eq!(h.take_tx().len(), 1);
        h.on_frame(t, CTRL, &frame(0x84, 0, 200, &[]));
        assert!(h.take_tx().is_empty());
    }

    #[test]
    fn usb_response_event_and_seq_dedupe() {
        let (mut h, t) = connected();
        h.take_tx();
        // GET_DESC_RSP: app, seq, type, req_id, offset, total, rcode, 7 bytes of data
        let rsp = |seq: &str| hex(&format!("3110{}{seq}02010000120000{}", "01", "12011001000000"));
        let ev = h.on_frame(t, CTRL, &frame(0x94, 0, 300, &rsp("01")));
        assert!(matches!(&ev[..], [Event::Usb(usb::Message::GetDesc { req_id: 1, total_size: 0x12, .. })]), "{ev:?}");
        // same element seq in a new frame: dropped
        assert!(h.on_frame(t, CTRL, &frame(0x94, 0, 301, &rsp("01"))).is_empty());
        assert_eq!(h.on_frame(t, CTRL, &frame(0x94, 0, 302, &rsp("02"))).len(), 1);
    }

    #[test]
    fn frames_from_strangers_are_ignored() {
        let (mut h, t) = connected();
        h.take_tx();
        let other = [0x02, 0, 0, 0, 0, 1];
        assert!(h.on_frame(t, other, &frame(0x84, 0, 5, &hex("3103010000"))).is_empty());
        assert!(h.take_tx().is_empty());
    }

    #[test]
    fn many_requests_pack_and_cap_in_flight() {
        let (mut h, t) = connected();
        h.take_tx();
        // 12-byte elements, 43 per 528-byte frame -> needs 5 frames
        for i in 0..200 {
            h.submit(&usb::Request::hid_get_report(i, 0, usb::REPORT_INPUT, 1));
        }
        h.on_frame(t, CTRL, &frame(0x84, 0, 400, &[]));
        let tx = h.take_tx();
        assert_eq!(tx.len(), MAX_UNACKED);
        for (i, (_, f)) in tx.iter().enumerate() {
            assert!(f.len() <= 528);
            let more = f[0] & ctl::MORE_DATA != 0;
            assert_eq!(more, i < MAX_UNACKED - 1);
        }
    }

    #[test]
    fn retire_handles_wrap() {
        let (mut h, t) = connected();
        h.take_tx();
        let s = h.session.as_mut().unwrap();
        s.last_tx_pkt = 254;
        h.on_frame(t, CTRL, &frame(0x84, 0, 500, &[])); // sends 255
        h.take_tx();
        h.on_frame(t, CTRL, &frame(0x94, 255, 501, &[])); // acks 255, sends 256
        let tx = h.take_tx();
        assert_eq!(&tx[0].1[2..6], &256u32.to_le_bytes());
        h.on_frame(t, CTRL, &frame(0x94, 0, 502, &[])); // 256 & 0xff == 0
        let tx = h.take_tx();
        assert_eq!(&tx[0].1[2..6], &257u32.to_le_bytes(), "256 retired");
    }

    #[test]
    fn sleep_then_stop() {
        let (mut h, t) = connected();
        assert!(h.poll(t + Duration::from_secs(3)).is_empty());
        assert_eq!(h.poll(t + Duration::from_secs(4)), [Event::Asleep]);
        assert!(h.poll(t + Duration::from_secs(9)).is_empty());
        assert_eq!(h.poll(t + Duration::from_secs(10)), [Event::Stopped]);
        assert_eq!(h.peer(), None);
    }

    #[test]
    fn reconnect_gets_new_session() {
        let (mut h, t) = connected();
        h.take_tx();
        let ev = h.on_frame(t, CTRL, &tablet_connect_req_frame());
        assert!(matches!(ev[..], [Event::Connected { session_id: 3, .. }]), "{ev:?}");
    }
}
