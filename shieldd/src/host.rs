//! The Windows host: Wi-Fi Direct GO (WinRT) -> raw Ozmo frames (Npcap) ->
//! ozproto session -> controller setup + HID reports -> outputs, w/
//! `--wifi-direct`. Also dispatches the default rtlusb host (radio.rs) and
//! `--receiver` (receiver.rs).
//!
//! The only other threads are Npcap receive and send and the WinRT status
//! callback; they just move frames and errors to and from the main thread,
//! which owns all state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use ozproto::{Config, Event, Host, Mac};
use tracing::{debug, info, trace, warn};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

use crate::adapters::fmt_mac;
use crate::controller::{Controller, Report};
use crate::outputs::Outputs;

use crate::{Args, adapters, hex, npcap, p2pconf, receiver, wifi};

const TICK: Duration = Duration::from_millis(10);
const SLOW_POLL: Duration = Duration::from_millis(100);
/// No frames for this long while connected: let go of everything, so a
/// dropout can't leave a stick or button held
const STALE: Duration = Duration::from_secs(1);

enum Msg {
    Frame(Mac, Vec<u8>, SystemTime),
    RxFailed(anyhow::Error),
    TxFailed(anyhow::Error),
}

pub fn run(args: Args, quit: Arc<AtomicBool>) -> Result<()> {
    if args.receiver {
        let mut outputs = Outputs::new(args.scale())?;
        return receiver::run(args.listen, &mut outputs, &quit);
    }
    if !args.wifi_direct {
        return crate::radio::run(args, &quit);
    }

    // Join the MTA explicitly rather than relying on the implicit one.
    //todo: --anchor-ssid hangs in WiFiAdapter.ConnectAsync().join() after the link
    //  is up, w/ or w/o this. Anchoring is a stopgap anyway (HANDOFF §9b).
    // SAFETY: first thing on this thread; never uninitialized, the process owns it
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("RoInitialize")?;
    npcap::init_dll_path()?;
    let mut outputs = Outputs::new(args.scale())?;
    let group = p2pconf::read(&args.conf)?;
    let radio = adapters::by_name(&args.adapter)?;
    let radio_mac = radio.mac.context("adapter has no MAC")?;
    info!(adapter = %args.adapter, mac = %fmt_mac(&radio_mac), "using adapter");

    wifi::ensure_radio_on(&args.adapter)?;
    if let Some(ssid) = &args.anchor_ssid {
        wifi::anchor(radio.bare_guid(), ssid, args.anchor_mhz)?;
    }
    let go = wifi::GroupOwner::start(&group.ssid, &group.passphrase)?;
    let go_if = adapters::wait_for_go(radio_mac, Duration::from_secs(10))?;
    info!(interface = %go_if.friendly_name, "GO interface up");

    let (tx, rx) = mpsc::channel();
    // Sends get their own thread: one Npcap send can block for 100+ ms while
    // the Realtek driver holds the frame for the dozing controller, and the
    // main loop stalling on that is what made input lag.
    let tx_stats = Arc::new(TxStats::default());
    let (out_tx, out_rx) = mpsc::channel::<Batch>();
    let tx_thread = {
        let mut link_tx = npcap::Link::open_tx(&go_if.guid, radio_mac)?;
        let tx = tx.clone();
        let stats = tx_stats.clone();
        std::thread::Builder::new().name("npcap-tx".into()).spawn(move || {
            // Ends when the main thread drops out_tx
            while let Ok(first) = out_rx.recv() {
                let queued_at = first.at;
                let frames = latest(first, &out_rx, &stats);
                stats.wait(queued_at.elapsed());
                for (dst, frame) in frames {
                    let t = Instant::now();
                    if let Err(e) = link_tx.send(dst, &frame) {
                        let _ = tx.send(Msg::TxFailed(e));
                        return;
                    }
                    stats.send(t.elapsed());
                }
            }
        })?
    };
    let rx_thread = {
        let mut link_rx = npcap::Link::open_rx(&go_if.guid, radio_mac)?;
        let quit = quit.clone();
        std::thread::Builder::new().name("npcap-rx".into()).spawn(move || {
            while !quit.load(Ordering::Relaxed) {
                match link_rx.recv() {
                    Ok(Some((src, frame, at))) => {
                        if tx.send(Msg::Frame(src, frame, at)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = tx.send(Msg::RxFailed(e));
                        break;
                    }
                }
            }
        })?
    };
    info!("waiting for the controller; tap the NVIDIA button. ctrl+c to stop");

    let mut host = Host::new(Config::default());
    let mut ctl = Controller::new(args.mic);
    let mut last_slow = Instant::now();
    // Last frame from the controller; None once outputs have been released
    let mut last_rx: Option<Instant> = None;
    let mut timing = Timing::default();

    let result = (|| -> Result<()> {
        while !quit.load(Ordering::Relaxed) {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Frame(src, payload, at)) => {
                    let now = Instant::now();
                    timing.queued(SystemTime::now().duration_since(at).unwrap_or_default());
                    last_rx = Some(now);
                    trace!(src = %fmt_mac(&src), frame = %hex(&payload), "rx");
                    for ev in host.on_frame(now, src, &payload) {
                        on_event(ev, now, &mut host, &mut ctl, &mut outputs);
                    }
                    timing.handled(now.elapsed());
                }
                Ok(Msg::RxFailed(e)) => return Err(e.context("capture thread")),
                Ok(Msg::TxFailed(e)) => return Err(e.context("send thread")),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();
            outputs.poll(now);
            if now.duration_since(last_slow) >= SLOW_POLL {
                last_slow = now;
                for ev in host.poll(now) {
                    on_event(ev, now, &mut host, &mut ctl, &mut outputs);
                }
                if host.peer().is_some() {
                    ctl.poll(now, &mut host);
                }
                if let Some(t) = last_rx
                    && now.duration_since(t) > STALE
                {
                    debug!("no frames for {STALE:?}, releasing outputs");
                    outputs.release_all();
                    last_rx = None;
                }
                timing.report(now, &tx_stats);
                if let Some(err) = go.stopped() {
                    anyhow::bail!("group owner stopped ({err:?})");
                }
            }

            let frames = host.take_tx();
            if !frames.is_empty() {
                for (dst, frame) in &frames {
                    trace!(dst = %fmt_mac(dst), frame = %hex(frame), "tx");
                }
                // A dead send thread has already queued its error
                let _ = out_tx.send(Batch { at: Instant::now(), frames });
            }
        }
        Ok(())
    })();

    info!("shutting down");
    quit.store(true, Ordering::Relaxed);
    drop(out_tx);
    let _ = tx_thread.join();
    let _ = rx_thread.join();
    result
}

pub(crate) fn on_event(ev: Event, now: Instant, host: &mut Host, ctl: &mut Controller, outputs: &mut Outputs) {
    match ev {
        Event::Connected { peer, session_id, .. } => {
            info!(peer = %fmt_mac(&peer), session_id, "controller connected");
            outputs.release_all();
            ctl.start(now, host);
        }
        Event::Resumed { session_id } => info!(session_id, "controller resumed session"),
        Event::Rejected { peer, status } => {
            warn!(peer = %fmt_mac(&peer), status, "connect rejected")
        }
        Event::Usb(msg) => {
            let was_ready = ctl.ready();
            for report in ctl.on_usb(now, &msg, host) {
                trace!(?report, "report");
                match report {
                    Report::Pad(p) => outputs.pad(&p),
                    Report::Trackpad(t) => outputs.trackpad(&t),
                }
            }
            if ctl.ready()
                && !was_ready
                && let Err(e) = outputs.plug()
            {
                warn!("{e:#}");
            }
        }
        Event::Serial(data) => debug!(data = %hex(&data), "serial app data"),
        Event::Farewell { ep, index, report } => {
            debug!(ep, index, report = %hex(&report), "farewell report")
        }
        Event::UpdateParam(p) => debug!(?p, "update param"),
        Event::Disconnect => {
            info!("controller going to sleep");
            outputs.release_all();
        }
        // The controller asks for a 300 ms presleep and then routinely goes
        // quiet that long, so this fires every second or so. Releasing here
        // snapped held sticks to neutral; the stale check in main() covers
        // real dropouts instead.
        Event::Asleep => trace!("controller idle"),
        // The pad stays plugged in until shieldd exits, so a dropout doesn't
        // make it vanish from Windows and come back as a new device
        Event::Stopped => {
            info!("controller session ended");
            outputs.release_all();
        }
    }
}

//region Send thread

/// Frames from one pass of the main loop
struct Batch {
    at: Instant,
    frames: Vec<(Mac, Vec<u8>)>,
}

/// Collapses a send backlog down to the newest batch. Every reply to a
/// trigger already carries everything still unacknowledged, so older
/// replies are stale; frames that are never retried, packet number 0 like
/// CONNECT_RSP, still go out.
fn latest(mut batch: Batch, rx: &mpsc::Receiver<Batch>, stats: &TxStats) -> Vec<(Mac, Vec<u8>)> {
    let mut once = Vec::new();
    while let Ok(newer) = rx.try_recv() {
        once.extend(batch.frames.into_iter().filter(|(_, f)| !retried(f)));
        stats.skipped.fetch_add(1, Ordering::Relaxed);
        batch = newer;
    }
    once.extend(batch.frames);
    once
}

fn retried(frame: &[u8]) -> bool {
    frame.len() >= 6 && frame[2..6] != [0; 4]
}

/// Worst cases from the send thread, in microseconds; read and reset once a second
#[derive(Default)]
struct TxStats {
    /// Main loop queued a batch -> send thread starts on it
    wait_us: AtomicU64,
    /// One Npcap send
    send_us: AtomicU64,
    /// Stale batches dropped
    skipped: AtomicU64,
}

impl TxStats {
    fn wait(&self, d: Duration) {
        self.wait_us.fetch_max(d.as_micros() as u64, Ordering::Relaxed);
    }

    fn send(&self, d: Duration) {
        self.send_us.fetch_max(d.as_micros() as u64, Ordering::Relaxed);
    }
}

//endregion

/// Worst-case latencies inside shieldd, logged once a second at debug level
#[derive(Default)]
struct Timing {
    since: Option<Instant>,
    frames: u32,
    /// Driver arrival -> main loop picks the frame up
    queued: Duration,
    /// Session + controller + outputs for one frame
    handled: Duration,
}

impl Timing {
    fn queued(&mut self, d: Duration) {
        self.frames += 1;
        self.queued = self.queued.max(d);
    }

    fn handled(&mut self, d: Duration) {
        self.handled = self.handled.max(d);
    }

    fn report(&mut self, now: Instant, tx: &TxStats) {
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) < Duration::from_secs(1) {
            return;
        }
        if self.frames > 0 {
            let us = |a: &AtomicU64| Duration::from_micros(a.swap(0, Ordering::Relaxed));
            debug!(
                frames = self.frames,
                max_queued = ?self.queued,
                max_handled = ?self.handled,
                max_tx_wait = ?us(&tx.wait_us),
                max_send = ?us(&tx.send_us),
                skipped = tx.skipped.swap(0, Ordering::Relaxed),
                "timing"
            );
        }
        *self = Timing { since: Some(now), ..Timing::default() };
    }
}
