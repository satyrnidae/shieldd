//! One controller's share of the host: its Ozmo session, setup and battery, its virtual pad,
//! and what's on its way to it. radio.rs keeps one per controller, up to MAX_SEATS.

use std::time::{Duration, Instant};

use anyhow::Result;
use ozproto::wire::ETHERTYPE;
use ozproto::{Config, Event, Host};
use rtlusb::Link;
use rtlusb::bss::Mac;
use shieldtray::Connected;
use shieldutl::mac::fmt_mac;
use shieldutl::vban;
use tracing::{debug, info, trace, warn};

use crate::controller::{Controller, HEADSET_EP, Report};
use crate::hex;
use crate::outputs::{Desktop, Gamepad};

/// XInput's four ports
pub const MAX_SEATS: usize = 4;
/// How long a DISCONNECT goodbye gets to be acknowledged before the kick goes out anyway.
/// The controller acks it and carries on, so it only goes first as a courtesy.
const GOODBYE_WAIT: Duration = Duration::from_secs(1);

/// A controller being sent away: a DISCONNECT goodbye, then a kick, a shun and the end of its
/// session once it's acknowledged or GOODBYE_WAIT runs out
#[derive(Debug, Clone, Copy)]
pub struct Leaving {
    pub since: Instant,
    pub deadline: Instant,
}

impl Leaving {
    pub fn new(now: Instant) -> Self {
        Leaving { since: now, deadline: now + GOODBYE_WAIT }
    }
}

pub struct Seat {
    pub mac: Mac,
    pub host: Host,
    pub ctl: Controller,
    /// Plugged in from its first setup until the run ends, so a dropout doesn't make it vanish
    /// from Windows and come back as a new device
    pub pad: Gamepad,
    /// Rumble waiting for the controller to take the previous one
    rumble: Option<[u16; 3]>,
    pub last_rx: Option<Instant>,
    /// Headset audio's next isoc frame number
    isoc_frame: u8,
    pub leaving: Option<Leaving>,
    /// Since it associated, until it's up with a battery reading
    pub joining: Option<Instant>,
    /// JOIN_TIMEOUT ran out on it this session; no spinner for it again
    pub join_gave_up: bool,
    /// Its mic and headset are on the shared VBAN streams
    pub vban: bool,
    /// Its virtual pad couldn't be plugged in, and why; radio.rs sends it away
    pub pad_failed: Option<String>,
}

impl Seat {
    pub fn new(mac: Mac, mic: bool, vban: bool) -> Self {
        let mut ctl = Controller::new(mic);
        ctl.set_streams(mic || vban, vban);
        Seat {
            mac,
            host: Host::new(Config::default()),
            ctl,
            pad: Gamepad::new(),
            rumble: None,
            last_rx: None,
            isoc_frame: 0,
            leaving: None,
            joining: None,
            join_gave_up: false,
            vban,
            pad_failed: None,
        }
    }

    /// VBAN on or off for it; `mic` is --mic, which keeps the mic open either way
    pub fn set_vban(&mut self, on: bool, mic: bool) {
        self.vban = on;
        self.ctl.set_streams(mic || on, on);
    }

    /// Up: an Ozmo session with a battery reading
    pub fn up(&self) -> bool {
        self.host.peer().is_some() && self.ctl.battery().is_some()
    }

    /// What the tray lists for it, if it has a session
    pub fn connected(&mut self, name: Option<String>) -> Option<Connected> {
        self.host.peer()?;
        Some(Connected { mac: self.mac, name, port: self.pad.port(), battery: self.ctl.battery(), vban: self.vban })
    }

    pub fn on_frame(&mut self, now: Instant, payload: &[u8], desktop: &mut Desktop) {
        self.last_rx = Some(now);
        for ev in self.host.on_frame(now, self.mac, payload) {
            self.on_event(ev, now, desktop);
        }
    }

    /// Ozmo retries and keep-alives, setup, status and the rest of what the controller needs
    pub fn poll(&mut self, now: Instant, desktop: &mut Desktop) {
        for ev in self.host.poll(now) {
            self.on_event(ev, now, desktop);
        }
        if self.host.peer().is_some() {
            self.ctl.poll(now, &mut self.host);
        }
    }

    /// Neutral pad, and its keys and buttons up
    pub fn release(&mut self, desktop: &mut Desktop) {
        self.pad.release();
        desktop.release(&self.mac);
    }

    /// Ends the session at once, after a kick
    pub fn end_session(&mut self, now: Instant, desktop: &mut Desktop) {
        if let Some(ev) = self.host.end_session() {
            self.on_event(ev, now, desktop);
        }
    }

    /// A game's rumble, out once the controller took the last one
    pub fn rumble(&mut self, now: Instant) {
        // Value 1 is the left motor, 3 the right, as an Xbox pad's large and small; 2 is
        // somewhere in the middle and goes unused
        if let Some((large, small)) = self.pad.take_rumble() {
            self.rumble = Some([u16::from(large) * 257, 0, u16::from(small) * 257]);
        }
        // Stale by the time a dropped controller is back; the game sends again on its next change
        if !self.ctl.ready() {
            self.rumble = None;
        }
        if let Some(v) = self.rumble
            && self.ctl.set_output(now, &mut self.host, v)
        {
            debug!(sta = %fmt_mac(&self.mac), large = v[0] >> 8, small = v[2] >> 8, "rumble");
            self.rumble = None;
        }
    }

    /// Wants headset audio: VBAN on and its headset stream open
    pub fn wants_headset(&self) -> bool {
        self.vban && self.ctl.headset_open()
    }

    /// One isoc unit of headset audio, UNIT_MS long
    pub fn send_headset(&mut self, pcm: &[u8]) {
        self.host.send_isoc(HEADSET_EP, self.isoc_frame, vban::UNIT_MS, pcm);
        self.isoc_frame = self.isoc_frame.wrapping_add(vban::UNIT_MS);
    }

    /// Everything the session has to send
    pub fn flush(&mut self, link: &mut Link) -> Result<()> {
        for (dst, frame) in self.host.take_tx() {
            // Kicked for a flat battery, the session's still open and still sending, headset
            // audio included. Nowhere to go, and not worth an error each tick.
            if !link.is_connected(&dst) {
                trace!(dst = %fmt_mac(&dst), "tx dropped, not connected");
                continue;
            }
            trace!(dst = %fmt_mac(&dst), frame = %hex(&frame), "tx");
            // Isoc audio is never retransmitted by Ozmo either; a late unit is no use
            if frame.first().is_some_and(|c| c & ozproto::wire::ctl::ISOC != 0) {
                link.send_stream(&dst, ETHERTYPE, &frame)?;
            } else {
                link.send(&dst, ETHERTYPE, &frame)?;
            }
        }
        Ok(())
    }

    fn on_event(&mut self, ev: Event, now: Instant, desktop: &mut Desktop) {
        let sta = fmt_mac(&self.mac);
        match ev {
            Event::Connected { peer, session_id, .. } => {
                info!(peer = %fmt_mac(&peer), session_id, "controller connected");
                self.release(desktop);
                self.ctl.start(now, &mut self.host);
            }
            Event::Resumed { session_id } => info!(%sta, session_id, "controller resumed session"),
            Event::Rejected { peer, status } => {
                warn!(peer = %fmt_mac(&peer), status, "connect rejected")
            }
            Event::Usb(msg) => {
                let was_ready = self.ctl.ready();
                for report in self.ctl.on_usb(now, &msg, &mut self.host) {
                    trace!(%sta, ?report, "report");
                    match report {
                        Report::Pad(p) => {
                            self.pad.pad(&p);
                            desktop.set_keys(&self.mac, p.keys);
                        }
                        Report::Trackpad(t) => desktop.trackpad(&self.mac, &t),
                    }
                }
                if self.ctl.ready()
                    && !was_ready
                    && let Err(e) = self.pad.plug()
                {
                    self.pad_failed = Some(format!("{e:#}"));
                }
            }
            Event::Serial(data) => debug!(%sta, data = %hex(&data), "serial app data"),
            Event::Farewell { ep, index, report } => {
                debug!(%sta, ep, index, report = %hex(&report), "farewell report")
            }
            Event::UpdateParam(p) => debug!(%sta, ?p, "update param"),
            Event::Disconnect => {
                info!(%sta, "controller going to sleep");
                self.release(desktop);
            }
            // The controller asks for a 300 ms presleep and then routinely goes
            // quiet that long, so this fires every second or so. Releasing here
            // snapped held sticks to neutral; the STALE check covers real dropouts instead.
            Event::Asleep => trace!(%sta, "controller idle"),
            Event::Stopped => {
                info!(%sta, "controller session ended");
                self.release(desktop);
            }
        }
    }
}
