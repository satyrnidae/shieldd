//! `--radio`: the controller on our own RTL8811AU driver (rtlusb) instead of Windows Wi-Fi
//! Direct + Npcap. A frame from the controller and our reply to it are handled on one thread
//! with no driver queue in between, which is what the Windows stack's ~330 ms of downlink lag
//! comes from.
//!
//! Same session, controller and outputs path as host.rs; only the link differs.
//!
//! `--pair`: the same, but on a fresh group (pair.rs) w/ WPS push-button on. The group goes
//! to --conf once the controller has it, and hosting carries on when it rejoins.
//!
//! `--background`: hosting in a loop that waits for the adapter and starts over when it fails.
//!
//! The tray's menu lands here too: pairing restarts the run on a new group, and VBAN feeds
//! the mic out and the headset in (vban.rs).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ozproto::wire::ETHERTYPE;
use ozproto::{Config, Host};
use rtlusb::ap::WpsConfig;
use rtlusb::wps::{Credential, Device};
use rtlusb::{ApOptions, Radio, RadioEvent};
use tracing::{debug, info, trace, warn};

use crate::adapters::fmt_mac;
use crate::controller::{Controller, HEADSET_EP, RumbleProbe};
use crate::host::on_event;
use crate::outputs::Outputs;
use crate::tray::{self, Command, Connected, Shunned, Status, Tray};
use crate::vban::{self, Vban};
use crate::{Args, hex, known_clients, p2pconf, pair};

/// WPS walk time: how long push-button stays on
const PAIR_WINDOW: Duration = Duration::from_secs(120);
const SLOW_POLL: Duration = Duration::from_millis(100);
/// No frames for this long while connected: let go of everything
const STALE: Duration = Duration::from_secs(1);
/// Hold replies for the dozing controller up to this long (rtlusb bss.rs). Override w/
/// SHIELDD_PS_HOLD_MS, 0 to send at once.
const PS_HOLD: Duration = Duration::from_millis(30);
/// --background: how often to look for the adapter
const ADAPTER_EVERY: Duration = Duration::from_secs(2);
/// --background: waits before starting over after a failure, doubling up to the max. A run
/// that lasted longer than the max starts again from the min.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How long a DISCONNECT goodbye gets to be acknowledged before the kick goes out anyway.
/// The controller acks it and carries on, so it only goes first as a courtesy.
const GOODBYE_WAIT: Duration = Duration::from_secs(1);
/// A controller sent away is ignored this long, so it gives up and sleeps: its sleep timer is
/// 30 s, plus some slack. Kicked w/o this, it rejoins within ~130 ms.
const SLEEP_SHUN: Duration = Duration::from_millis(30_250);
/// A controller that associated, or is connected w/o a battery reading, shows as joining until
/// it's up, or this long
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Retry saving a just-paired group this often until it works
const SAVE_RETRY: Duration = Duration::from_secs(5);
/// Headset audio owed after a stall goes, past this much
const ISOC_CREDIT_MAX: Duration = Duration::from_millis(40);

/// A controller being sent away: a DISCONNECT goodbye, then a kick, SLEEP_SHUN and the end of
/// its session once it's acknowledged or GOODBYE_WAIT runs out
#[derive(Debug, Clone, Copy)]
struct Leaving {
    since: Instant,
    deadline: Instant,
}

impl Leaving {
    fn new(now: Instant) -> Self {
        Leaving { since: now, deadline: now + GOODBYE_WAIT }
    }
}

/// Why a run ended
enum Next {
    Quit,
    /// Pair from the tray: a new run, on a new group
    Pair,
    /// Tray pairing timed out: back to the saved group
    Host,
}

/// What outlives a run: the tray, VBAN w/ its sockets, and a paired group --conf wouldn't take
struct Shared {
    tray: Tray,
    vban: Option<Vban>,
    /// Hosted in place of --conf's, and saved again each run until it works
    unsaved: Option<p2pconf::Group>,
    /// --conf, which known-clients sits next to
    conf: PathBuf,
}

impl Shared {
    fn vban_marker() -> PathBuf {
        crate::settings_dir().join("vban.on")
    }

    /// Starts or stops VBAN, and remembers it for next time
    fn set_vban(&mut self, on: bool) {
        if on == self.vban.is_some() {
            return;
        }
        if on {
            match Vban::start() {
                Ok(v) => self.vban = Some(v),
                Err(e) => {
                    warn!("VBAN: {e:#}");
                    self.tray.problem("VBAN failed", &format!("{e:#}"));
                    return;
                }
            }
        } else {
            self.vban = None;
        }
        let marker = Self::vban_marker();
        let saved = if on {
            std::fs::create_dir_all(crate::settings_dir()).and_then(|()| std::fs::write(&marker, b""))
        } else {
            std::fs::remove_file(&marker)
        };
        if let Err(e) = saved {
            debug!("remembering VBAN {on}: {e}");
        }
    }

    /// Saves a controller's new name, or says why it couldn't. Returns whether it did.
    fn rename(&self, mac: &[u8; 6], name: &str) -> bool {
        match known_clients::save(&self.conf, mac, name) {
            Ok(()) => {
                info!(sta = %fmt_mac(mac), name, "renamed");
                true
            }
            Err(e) => {
                warn!("renaming the controller: {e:#}");
                self.tray.problem("Couldn't rename the controller", &format!("{e:#}"));
                false
            }
        }
    }

    /// Between runs, w/ no radio: Exit, VBAN, and a rename the dialog was still open for
    fn idle(&mut self, quit: &AtomicBool, d: Duration) {
        let until = Instant::now() + d;
        while !quit.load(Ordering::Relaxed) && Instant::now() < until {
            for cmd in self.tray.commands() {
                match cmd {
                    Command::Exit => quit.store(true, Ordering::Relaxed),
                    Command::Vban(on) => self.set_vban(on),
                    Command::Pair => self.tray.problem("Can't pair yet", "Plug in the adapter first."),
                    Command::Rename(mac, name) => {
                        self.rename(&mac, &name);
                    }
                    Command::Identify | Command::Disconnect => {}
                }
            }
            self.tray.update(Status {
                controller: None,
                vban: self.vban.is_some(),
                pairing: false,
                ssid: None,
                channel: None,
                joining: false,
                shunned: None,
            });
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn run(args: Args, quit: &AtomicBool) -> Result<()> {
    // Up the whole time, so it also says shieldd is running while there's nothing to host
    let mut sh = Shared { tray: Tray::start(), vban: None, unsaved: None, conf: args.conf.clone() };
    if Shared::vban_marker().exists() {
        sh.set_vban(true);
    }
    let mut pair = args.pair;
    // Only `shieldd --pair` itself gives up when nobody pairs; from the tray it's back to hosting
    let mut pair_bails = args.pair && !args.background;
    let mut backoff = RETRY_MIN;
    let mut waiting = false;
    while !quit.load(Ordering::Relaxed) {
        if args.background && !rtlusb::usb::present() {
            if !waiting {
                info!("waiting for the adapter");
                waiting = true;
            }
            sh.idle(quit, ADAPTER_EVERY);
            continue;
        }
        waiting = false;
        let started = Instant::now();
        let next = run_once(&args, pair, pair_bails, quit, &mut sh);
        pair_bails = false;
        match next {
            Ok(Next::Quit) => break,
            Ok(Next::Pair) => pair = true,
            Ok(Next::Host) => pair = false,
            Err(e) if !args.background => return Err(e),
            Err(e) => {
                if started.elapsed() > RETRY_MAX {
                    backoff = RETRY_MIN;
                }
                warn!("{e:#}; starting over in {backoff:?}");
                pair = false;
                sh.idle(quit, backoff);
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
    Ok(())
}

fn run_once(args: &Args, pair: bool, pair_bails: bool, quit: &AtomicBool, sh: &mut Shared) -> Result<Next> {
    let mut outputs = Outputs::new(args.scale())?;
    let mut group = match &sh.unsaved {
        _ if pair => pair::new_group(),
        Some(g) => g.clone(),
        None => p2pconf::read(&args.conf)?,
    };
    let wps = pair.then(|| WpsConfig {
        device: Device::from_seed(group.ssid.as_bytes(), "shieldd"),
        credential: Credential { ssid: group.ssid.clone(), passphrase: group.passphrase.clone() },
    });
    let pmk = rtlusb::wpa::pmk(&group.passphrase, &group.ssid);
    let ps_hold = match std::env::var("SHIELDD_PS_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(PS_HOLD),
    };
    info!(?ps_hold, "power save hold");
    let opts =
        ApOptions { ssid: group.ssid.clone(), channel: group.channel, bssid: None, pmk, ps_hold, hidden: true, wps };
    sh.tray.update(Status {
        controller: None,
        vban: sh.vban.is_some(),
        pairing: pair,
        ssid: Some(group.ssid.clone()),
        channel: (group.channel != 0).then_some(group.channel),
        joining: false,
        shunned: None,
    });
    let mut radio = Radio::start(opts)?;
    // Pairing surveys for its channel; that's the one the controller will remember
    group.channel = radio.channel();
    let channel = group.channel;
    let bssid = fmt_mac(&radio.bssid());
    if pair {
        info!(ssid = group.ssid, %bssid, channel, "pairing: hold the NVIDIA button until the light flashes");
    } else {
        info!(ssid = group.ssid, %bssid, channel, "waiting for the controller; tap the NVIDIA button");
    }

    let mut host = Host::new(Config::default());
    let mut ctl = Controller::new(args.mic);
    let vban_on = sh.vban.is_some();
    ctl.set_streams(args.mic || vban_on, vban_on);
    let mut probe = RumbleProbe::from_env();
    // Rumble waiting for the controller to take the previous one
    let mut rumble: Option<[u16; 3]> = None;
    let mut last_slow = Instant::now();
    let mut last_rx: Option<Instant> = None;
    let pair_until = pair.then(|| Instant::now() + PAIR_WINDOW);
    let mut paired = false;
    let mut pair_timed_out = false;
    // Headset audio: time owed in isoc units, since when, and the unit's frame number
    let mut isoc_credit = Duration::ZERO;
    let mut isoc_at: Option<Instant> = None;
    let mut isoc_frame: u8 = 0;
    let mut leaving: Option<Leaving> = None;
    let mut names = known_clients::load(&args.conf);
    // Since a controller associated, until it's up w/ a battery reading
    let mut joining: Option<Instant> = None;
    // JOIN_TIMEOUT ran out on the controller there now; no spinner for it again
    let mut join_gave_up = false;
    // Sent away to sleep, w/ its name looked up once. rtlusb's shun says how long.
    let mut shunned: Option<Shunned> = None;
    // Paired, but writing --conf failed: when to try again
    let mut save_due: Option<Instant> = sh.unsaved.is_some().then(Instant::now);
    // Ends this run but not the program, unlike quit
    let stop = AtomicBool::new(false);
    let mut next = Next::Quit;

    radio.run(&stop, &mut |ev, link| {
        let now = Instant::now();
        match ev {
            RadioEvent::Data { src, ethertype: ETHERTYPE, payload } => {
                last_rx = Some(now);
                trace!(src = %fmt_mac(&src), frame = %hex(payload), "rx");
                for ev in host.on_frame(now, src, payload) {
                    on_event(ev, now, &mut host, &mut ctl, &mut outputs);
                }
            }
            RadioEvent::Data { ethertype, .. } => {
                debug!(ethertype = format!("{ethertype:#06x}"), "ignoring data frame")
            }
            RadioEvent::Associated(_) => joining = Some(now),
            RadioEvent::Connected(sta) => {
                info!(sta = %fmt_mac(&sta), "Wi-Fi link up");
                tray::device_sound(true);
            }
            RadioEvent::Disconnected(sta) => {
                joining = None;
                join_gave_up = false;
                info!(sta = %fmt_mac(&sta), "Wi-Fi link down");
                tray::device_sound(false);
            }
            RadioEvent::Paired(sta, model) => {
                paired = true;
                // Named after its model the first time; a rename, or an earlier pairing, wins
                if !names.contains_key(&sta)
                    && let Some(name) = known_clients::clean(model)
                {
                    if let Err(e) = known_clients::save(&args.conf, &sta, &name) {
                        warn!("couldn't save the controller's name: {e:#}");
                    }
                    names.insert(sta, name);
                }
                // The controller has the credential now, so this group is the one it remembers
                match p2pconf::write(&args.conf, &group) {
                    Ok(backup) => info!(
                        sta = %fmt_mac(&sta),
                        ssid = group.ssid,
                        conf = %args.conf.display(),
                        backup = ?backup,
                        "paired; group saved, waiting for the controller to rejoin"
                    ),
                    Err(e) => {
                        // Hosted until shieldd exits, restarts included
                        warn!("paired, but saving the group failed: {e:#}; retrying every {SAVE_RETRY:?}");
                        sh.tray.problem(
                            "Couldn't save the paired network",
                            &format!("{e:#}\n\nThe controller works until shieldd exits. Retrying."),
                        );
                        sh.unsaved = Some(group.clone());
                        save_due = Some(now + SAVE_RETRY);
                    }
                }
            }
            RadioEvent::PairingFailed(sta, why) => {
                warn!(sta = %fmt_mac(&sta), "pairing failed: {why}; still listening, hold the button again")
            }
            RadioEvent::Tick => {
                if quit.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Relaxed);
                }
                if let Some(t) = pair_until
                    && !paired
                    && now >= t
                {
                    pair_timed_out = true;
                    next = Next::Host;
                    stop.store(true, Ordering::Relaxed);
                }
                if now.duration_since(last_slow) >= SLOW_POLL {
                    last_slow = now;
                    if let Some(t) = save_due
                        && now >= t
                    {
                        match p2pconf::write(&args.conf, &group) {
                            Ok(_) => {
                                info!(conf = %args.conf.display(), "group saved");
                                sh.unsaved = None;
                                save_due = None;
                            }
                            Err(e) => {
                                debug!("saving the group: {e:#}");
                                save_due = Some(now + SAVE_RETRY);
                            }
                        }
                    }
                    for ev in host.poll(now) {
                        on_event(ev, now, &mut host, &mut ctl, &mut outputs);
                    }
                    for cmd in sh.tray.commands() {
                        match cmd {
                            Command::Exit => {
                                quit.store(true, Ordering::Relaxed);
                                stop.store(true, Ordering::Relaxed);
                            }
                            Command::Pair => {
                                info!("pairing from the tray; letting go of the controller");
                                next = Next::Pair;
                                stop.store(true, Ordering::Relaxed);
                            }
                            Command::Vban(on) => {
                                sh.set_vban(on);
                                let on = sh.vban.is_some();
                                ctl.set_streams(args.mic || on, on);
                            }
                            Command::Identify => ctl.identify(),
                            Command::Rename(mac, name) => {
                                if sh.rename(&mac, &name) {
                                    names.insert(mac, name);
                                }
                            }
                            Command::Disconnect => {
                                if leaving.is_none() && host.say_goodbye() {
                                    info!("disconnecting from the tray; saying goodbye first");
                                    leaving = Some(Leaving::new(now));
                                }
                            }
                        }
                    }
                    if host.peer().is_some() {
                        ctl.poll(now, &mut host);
                        if let Some(p) = probe.as_mut() {
                            p.poll(now, &mut ctl, &mut host);
                        }
                    }
                    // Before the tray update, so it says "charge it" rather than "critical"
                    if leaving.is_none() && ctl.must_charge(now) && host.say_goodbye() {
                        warn!("battery too low to use; sending the controller away until it charges");
                        ctl.let_go();
                        leaving = Some(Leaving::new(now));
                    }
                    // Never on the tick it started, so the tray has seen the flat battery and said
                    // so before the controller's gone
                    if let Some(l) = leaving
                        && now > l.since
                        && (host.all_delivered() || now >= l.deadline)
                    {
                        leaving = None;
                        if let Some(peer) = host.peer() {
                            info!(sta = %fmt_mac(&peer), acked = host.all_delivered(), "goodbye done, kicking");
                            link.kick(&peer)?;
                            link.shun(&peer, SLEEP_SHUN);
                            shunned = Some(Shunned { mac: peer, name: names.get(&peer).cloned() });
                            if let Some(ev) = host.end_session() {
                                on_event(ev, now, &mut host, &mut ctl, &mut outputs);
                            }
                        }
                    }
                    if shunned.as_ref().is_some_and(|s| link.shunned() != Some(s.mac)) {
                        shunned = None;
                    }
                    let controller = host.peer().map(|mac| Connected {
                        mac,
                        name: names.get(&mac).cloned(),
                        port: outputs.port(),
                        battery: ctl.battery(),
                    });
                    let up = controller.as_ref().is_some_and(|c| c.battery.is_some());
                    let present = controller.is_some();
                    if up {
                        joining = None;
                        join_gave_up = false;
                    } else if joining.is_some_and(|t| now.duration_since(t) >= JOIN_TIMEOUT) {
                        warn!("controller hasn't come up in {JOIN_TIMEOUT:?}");
                        joining = None;
                        join_gave_up = present;
                    } else if present && joining.is_none() && !join_gave_up {
                        joining = Some(now);
                    }
                    // A new session gets its own JOIN_TIMEOUT, Wi-Fi link or not
                    if !present {
                        join_gave_up = false;
                    }
                    sh.tray.update(Status {
                        controller,
                        vban: sh.vban.is_some(),
                        pairing: pair && !paired,
                        ssid: Some(group.ssid.clone()),
                        channel: Some(channel),
                        joining: joining.is_some(),
                        shunned: shunned.clone(),
                    });
                    if let Some(t) = last_rx
                        && now.duration_since(t) > STALE
                    {
                        debug!("no frames for {STALE:?}, releasing outputs");
                        outputs.release_all();
                        last_rx = None;
                    }
                }
            }
        }
        let mic = ctl.take_mic();
        if let Some(v) = sh.vban.as_mut()
            && !mic.is_empty()
        {
            v.push_mic(&mic);
        }
        // Headset audio, one 4 ms unit per 4 ms of wall clock
        match sh.vban.as_ref().filter(|_| ctl.headset_open()) {
            Some(v) => {
                let at = isoc_at.replace(now).unwrap_or(now);
                isoc_credit = (isoc_credit + now.duration_since(at)).min(ISOC_CREDIT_MAX);
                let unit = Duration::from_millis(u64::from(vban::UNIT_MS));
                while isoc_credit >= unit {
                    isoc_credit -= unit;
                    if let Some(pcm) = v.take_unit() {
                        host.send_isoc(HEADSET_EP, isoc_frame, vban::UNIT_MS, &pcm);
                        isoc_frame = isoc_frame.wrapping_add(vban::UNIT_MS);
                    }
                }
            }
            None => {
                isoc_at = None;
                isoc_credit = Duration::ZERO;
            }
        }
        // Value 1 is the left motor, 3 the right, as an Xbox pad's large and small; 2 is
        // somewhere in the middle and goes unused
        if let Some((large, small)) = outputs.take_rumble() {
            rumble = Some([u16::from(large) * 257, 0, u16::from(small) * 257]);
        }
        // Stale by the time a dropped controller is back; the game sends again on its next change
        if !ctl.ready() {
            rumble = None;
        }
        if let Some(v) = rumble
            && ctl.set_output(now, &mut host, v)
        {
            debug!(large = v[0] >> 8, small = v[2] >> 8, "rumble");
            rumble = None;
        }
        outputs.poll(now);
        for (dst, frame) in host.take_tx() {
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
    })?;
    if pair_timed_out {
        if pair_bails {
            bail!("no controller paired within {PAIR_WINDOW:?}; --conf is unchanged");
        }
        warn!("no controller paired within {PAIR_WINDOW:?}; back to the saved group");
        sh.tray.problem("No controller paired", "Nothing paired in 2 minutes; back to the saved one.");
    }
    Ok(next)
}
