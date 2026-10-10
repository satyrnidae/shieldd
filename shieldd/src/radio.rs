//! The host: controllers on our own RTL8811AU driver (rtlusb). A frame from a controller and
//! our reply to it are handled on one thread with no driver queue in between; the Windows
//! Wi-Fi Direct stack's queue held replies for the dozing controller, ~330 ms of input lag.
//!
//! Up to four controllers at once, each with its own session and virtual pad (seat.rs), sharing
//! the mouse, media keys and VBAN streams.
//!
//! `--pair`: WPS push-button on the saved group until one controller has the credential, or
//! PAIR_WINDOW runs out. Nobody already connected is dropped.
//!
//! `--reconfigure-network`: a fresh group (pair.rs), saved to --conf at once, then pairing as
//! --pair. Every controller paired to the old group has to pair again.
//!
//! `--background`: hosting in a loop that waits for the adapter and starts over when it fails.
//!
//! The tray's menu lands here too: pairing, reconfiguring, and each controller's identify,
//! rename, VBAN and disconnect.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ozproto::Mac;
use ozproto::wire::ETHERTYPE;
use rtlusb::ap::WpsConfig;
use rtlusb::wps::{Credential, Device};
use rtlusb::{ApOptions, Link, Radio, RadioEvent};
use shieldtray::{self as tray, Command, Shunned, Status, Tray, tr};
use shieldutl::mac::fmt_mac;
use shieldutl::vban::{self, Vban};
use tracing::{debug, info, trace, warn};

use crate::outputs::Desktop;
use crate::seat::{Leaving, MAX_SEATS, Seat};
use crate::{Args, hex, known_clients, p2pconf, pair};

/// WPS walk time: how long push-button stays on
const PAIR_WINDOW: Duration = Duration::from_secs(120);
const SLOW_POLL: Duration = Duration::from_millis(100);
/// No frames for this long while connected: let go of everything
const STALE: Duration = Duration::from_secs(1);
/// Hold replies for a dozing controller up to this long (rtlusb bss.rs). Override with
/// SHIELDD_PS_HOLD_MS, 0 to send at once.
const PS_HOLD: Duration = Duration::from_millis(30);
/// --background: how often to look for the adapter
const ADAPTER_EVERY: Duration = Duration::from_secs(2);
/// --background: waits before starting over after a failure, doubling up to the max. A run
/// that lasted longer than the max starts again from the min.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A controller sent away is ignored this long, so it gives up and sleeps: its sleep timer is
/// 30 s, plus some slack. Kicked without this, it rejoins within ~130 ms.
const SLEEP_SHUN: Duration = Duration::from_millis(30_250);
/// A controller that associated, or is connected without a battery reading, shows as joining until
/// it's up, or this long
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Retry saving a new group this often until it works
const SAVE_RETRY: Duration = Duration::from_secs(5);
/// Headset audio owed after a stall goes, past this much
const ISOC_CREDIT_MAX: Duration = Duration::from_millis(40);

/// How a run starts
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Start {
    /// A new group, saved before anything else
    reconfigure: bool,
    /// WPS push-button on from the start
    pair: bool,
}

impl Start {
    const RECONFIGURE: Start = Start { reconfigure: true, pair: true };
}

/// Why a run ended
enum Next {
    Quit,
    /// Reconfigure from the tray: a new run, on a new group
    Reconfigure,
}

/// What outlives a run: the tray, VBAN with its sockets, and a new group --conf wouldn't take
struct Shared {
    tray: Tray,
    /// The shared VBAN streams, up while any controller has VBAN on
    vban: Option<Vban>,
    /// Controllers with VBAN on, connected or not
    vban_on: BTreeSet<Mac>,
    /// Hosted in place of --conf's, and saved again each run until it works
    unsaved: Option<p2pconf::Group>,
    /// --conf, which known-clients sits next to
    conf: PathBuf,
    /// --conf is the default one, whose key gets sealed
    seal: bool,
    /// Picked from the tray with no adapter to do it on; done once there is
    pending: Option<Start>,
}

impl Shared {
    fn vban_file() -> PathBuf {
        crate::settings_dir().join("vban-clients")
    }

    /// From before VBAN was per controller; which one it meant is anyone's guess
    fn old_vban_marker() -> PathBuf {
        crate::settings_dir().join("vban.on")
    }

    /// Controllers that had VBAN on last time, one MAC a line
    fn load_vban(&mut self) {
        if std::fs::remove_file(Self::old_vban_marker()).is_ok() {
            info!("VBAN is per controller now; turn it on again in each one's menu");
        }
        let text = std::fs::read_to_string(Self::vban_file()).unwrap_or_default();
        self.vban_on = text.lines().filter_map(|l| known_clients::parse_mac(l.trim())).collect();
        self.sync_vban();
    }

    /// The streams up iff any controller wants them
    fn sync_vban(&mut self) {
        if self.vban_on.is_empty() {
            self.vban = None;
        } else if self.vban.is_none() {
            match Vban::start() {
                Ok(v) => self.vban = Some(v),
                // Who wants it stays remembered; the next toggle tries again
                Err(e) => {
                    warn!("VBAN: {e:#}");
                    self.tray.problem(&tr!("problem-vban-title"), &format!("{e:#}"));
                }
            }
        }
    }

    /// Turns VBAN on or off for `mac`, and remembers it for next time. Returns whether it's on.
    fn set_vban(&mut self, mac: &Mac, on: bool) -> bool {
        let changed = if on { self.vban_on.insert(*mac) } else { self.vban_on.remove(mac) };
        if !on && let Some(v) = self.vban.as_mut() {
            v.forget_mic(mac);
        }
        self.sync_vban();
        if changed {
            let text: String = self.vban_on.iter().map(|m| format!("{}\n", fmt_mac(m))).collect();
            let saved = std::fs::create_dir_all(crate::settings_dir()).and_then(|()| match text.is_empty() {
                true => std::fs::remove_file(Self::vban_file()),
                false => std::fs::write(Self::vban_file(), text),
            });
            if let Err(e) = saved {
                debug!("remembering VBAN for {}: {e}", fmt_mac(mac));
            }
        }
        self.vban_on.contains(mac)
    }

    /// Saves a controller's new name, or says why it couldn't. Returns whether it did.
    fn rename(&self, mac: &Mac, name: &str) -> bool {
        match known_clients::save(&self.conf, mac, name) {
            Ok(()) => {
                info!(sta = %fmt_mac(mac), name, "renamed");
                true
            }
            Err(e) => {
                warn!("renaming the controller: {e:#}");
                self.tray.problem(&tr!("problem-rename-title"), &format!("{e:#}"));
                false
            }
        }
    }

    /// Between runs, with no radio: Exit, VBAN, a rename the dialog was still open for, and
    /// pairing or reconfiguring for once there's an adapter
    fn idle(&mut self, quit: &AtomicBool, d: Duration) {
        let until = Instant::now() + d;
        while !quit.load(Ordering::Relaxed) && Instant::now() < until {
            for cmd in self.tray.commands() {
                match cmd {
                    Command::Exit => quit.store(true, Ordering::Relaxed),
                    Command::Vban(mac, on) => {
                        self.set_vban(&mac, on);
                    }
                    Command::Pair | Command::Reconfigure => {
                        let start = if cmd == Command::Pair {
                            Start { pair: true, ..Start::default() }
                        } else {
                            Start::RECONFIGURE
                        };
                        // A reconfigure already waiting covers a pair
                        if self.pending != Some(Start::RECONFIGURE) {
                            self.pending = Some(start);
                        }
                        // Between runs that failed, the adapter may well be in already
                        match rtlusb::usb::present() {
                            true => self.tray.problem(&tr!("problem-pair-soon-title"), &tr!("problem-pair-soon")),
                            false => self.tray.problem(&tr!("problem-no-adapter-title"), &tr!("problem-no-adapter")),
                        }
                    }
                    Command::Rename(mac, name) => {
                        self.rename(&mac, &name);
                    }
                    Command::Identify(_) | Command::Disconnect(_) => {}
                }
            }
            self.tray.update(Status::default());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn run(args: Args, quit: &AtomicBool) -> Result<()> {
    // Up the whole time, so it also says shieldd is running while there's nothing to host
    let mut sh = Shared {
        tray: Tray::start(crate::ABOUT),
        vban: None,
        vban_on: BTreeSet::new(),
        unsaved: None,
        conf: args.conf.clone(),
        seal: crate::is_default_conf(&args.conf),
        pending: None,
    };
    sh.load_vban();
    let mut start = Start { reconfigure: args.reconfigure_network, pair: args.pair || args.reconfigure_network };
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
        if let Some(p) = sh.pending.take() {
            start = p;
        }
        let started = Instant::now();
        let next = run_once(&args, start, quit, &mut sh);
        start = Start::default();
        match next {
            Ok(Next::Quit) => break,
            Ok(Next::Reconfigure) => start = Start::RECONFIGURE,
            Err(e) if !args.background => return Err(e),
            Err(e) => {
                if started.elapsed() > RETRY_MAX {
                    backoff = RETRY_MIN;
                }
                warn!("{e:#}; starting over in {backoff:?}");
                sh.idle(quit, backoff);
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
    Ok(())
}

/// WPS push-button handing out `group`
fn wps_for(group: &p2pconf::Group) -> Result<WpsConfig> {
    let passphrase = group
        .psk
        .passphrase()
        .context("the saved group has a key but no passphrase to hand out; reconfigure the network to pair")?;
    Ok(WpsConfig {
        device: Device::from_seed(group.ssid.as_bytes(), "shieldd"),
        credential: Credential { ssid: group.ssid.clone(), passphrase: passphrase.to_owned() },
    })
}

/// Saves `group` to --conf, or keeps it hosted unsaved. Returns when to try again. Only the first
/// try, `!retry`, says it failed.
fn save_group(args: &Args, sh: &mut Shared, group: &p2pconf::Group, now: Instant, retry: bool) -> Option<Instant> {
    match p2pconf::write(&args.conf, group, sh.seal) {
        Ok(backup) => {
            info!(ssid = group.ssid, conf = %args.conf.display(), backup = ?backup, "new group saved");
            sh.unsaved = None;
            None
        }
        Err(e) if retry => {
            debug!("saving the new group: {e:#}");
            Some(now + SAVE_RETRY)
        }
        Err(e) => {
            // Hosted until shieldd exits, restarts included
            warn!("saving the new group failed: {e:#}; retrying every {SAVE_RETRY:?}");
            sh.tray.problem(&tr!("problem-save-title"), &tr!("problem-save", error = format!("{e:#}")));
            sh.unsaved = Some(group.clone());
            Some(now + SAVE_RETRY)
        }
    }
}

/// The seat for `mac`, made if it's new. One past MAX_SEATS takes the place of one that's
/// gone, with no session and not even joining Wi-Fi, or is sent away to sleep if they're all here.
fn seat_for<'a>(
    seats: &'a mut Vec<Seat>,
    mac: Mac,
    args: &Args,
    sh: &Shared,
    link: &mut Link,
) -> Result<Option<&'a mut Seat>> {
    if let Some(i) = seats.iter().position(|s| s.mac == mac) {
        return Ok(Some(&mut seats[i]));
    }
    if seats.len() >= MAX_SEATS {
        match seats.iter().position(|s| s.host.peer().is_none() && !link.is_associated(&s.mac)) {
            Some(i) => {
                info!(gone = %fmt_mac(&seats[i].mac), new = %fmt_mac(&mac), "seat handed over");
                seats.remove(i);
            }
            None => {
                warn!(sta = %fmt_mac(&mac), "a fifth controller; sending it away");
                link.kick(&mac)?;
                link.shun(&mac, SLEEP_SHUN);
                sh.tray.problem(
                    &tr!("problem-too-many-title"),
                    &tr!("problem-too-many", max = MAX_SEATS, mac = fmt_mac(&mac)),
                );
                return Ok(None);
            }
        }
    }
    seats.push(Seat::new(mac, args.mic, sh.vban_on.contains(&mac)));
    Ok(seats.last_mut())
}

fn seat_mut<'a>(seats: &'a mut [Seat], mac: &Mac) -> Option<&'a mut Seat> {
    seats.iter_mut().find(|s| s.mac == *mac)
}

fn run_once(args: &Args, start: Start, quit: &AtomicBool, sh: &mut Shared) -> Result<Next> {
    let mut desktop = Desktop::new(args.scale());
    let reconfigure = start.reconfigure || (start.pair && sh.unsaved.is_none() && !args.conf.exists());
    let mut group = match &sh.unsaved {
        // An unsaved group stays until the new one's saved or takes its place, in case the radio
        // doesn't come up
        _ if reconfigure => pair::new_group(),
        Some(g) => g.clone(),
        None if !args.conf.exists() => bail!(
            "no saved group in {}; pair a controller first: shieldd --pair, or Pair controller in the tray",
            args.conf.display()
        ),
        None => p2pconf::read(&args.conf, sh.seal)?,
    };
    let wps = match start.pair || reconfigure {
        true => Some(wps_for(&group)?),
        false => None,
    };
    let pmk = rtlusb::wpa::pmk_of(&group.psk, &group.ssid);
    let ps_hold = match std::env::var("SHIELDD_PS_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(PS_HOLD),
    };
    info!(?ps_hold, "power save hold");
    let pairing = wps.is_some();
    let opts =
        ApOptions { ssid: group.ssid.clone(), channel: group.channel, bssid: None, pmk, ps_hold, hidden: true, wps };
    sh.tray.update(Status {
        pairing,
        ssid: Some(group.ssid.clone()),
        channel: (group.channel != 0).then_some(group.channel),
        ..Status::default()
    });
    let mut radio = Radio::start(opts)?;
    // A new group surveys for its channel; that's the one the controllers will remember
    group.channel = radio.channel();
    let channel = group.channel;
    let bssid = fmt_mac(&radio.bssid());
    // Saved before anyone pairs, so a controller that does is never left on a group shieldd forgot
    let mut save_due = match reconfigure {
        true => save_group(args, sh, &group, Instant::now(), false),
        false => sh.unsaved.is_some().then(Instant::now),
    };
    if pairing {
        info!(ssid = group.ssid, %bssid, channel, "pairing: hold the NVIDIA button until the light flashes");
    } else {
        info!(ssid = group.ssid, %bssid, channel, "waiting for controllers; tap the NVIDIA button");
    }

    let mut seats: Vec<Seat> = Vec::new();
    // Past the 4-way handshake: their leaving is worth a sound
    let mut linked: HashSet<Mac> = HashSet::new();
    let mut last_slow = Instant::now();
    let mut pair_until = pairing.then(|| Instant::now() + PAIR_WINDOW);
    // Headset audio: time owed in isoc units, and since when
    let mut isoc_credit = Duration::ZERO;
    let mut isoc_at: Option<Instant> = None;
    let mut names = known_clients::load(&args.conf);
    // Ends this run but not the program, unlike quit
    let stop = AtomicBool::new(false);
    let mut next = Next::Quit;

    radio.run(&stop, &mut |ev, link| {
        let now = Instant::now();
        match ev {
            RadioEvent::Data { src, ethertype: ETHERTYPE, payload } => {
                trace!(src = %fmt_mac(&src), frame = %hex(payload), "rx");
                if let Some(seat) = seat_for(&mut seats, src, args, sh, link)? {
                    seat.on_frame(now, payload, &mut desktop);
                }
            }
            RadioEvent::Data { ethertype, .. } => {
                debug!(ethertype = format!("{ethertype:#06x}"), "ignoring data frame")
            }
            RadioEvent::Associated(sta) => {
                if let Some(seat) = seat_for(&mut seats, sta, args, sh, link)? {
                    seat.joining = Some(now);
                }
            }
            RadioEvent::Connected(sta) => {
                info!(sta = %fmt_mac(&sta), "Wi-Fi link up");
                linked.insert(sta);
                // Its pad's first plug-in plays Windows' own sound a moment later; one's enough
                if seat_mut(&mut seats, &sta).is_some_and(|s| s.pad.plugged()) {
                    tray::device_sound(true);
                }
            }
            RadioEvent::Disconnected(sta) => {
                if let Some(seat) = seat_mut(&mut seats, &sta) {
                    seat.joining = None;
                    seat.join_gave_up = false;
                }
                // Not for one turned away before it ever connected
                if linked.remove(&sta) {
                    info!(sta = %fmt_mac(&sta), "Wi-Fi link down");
                    tray::device_sound(false);
                }
            }
            RadioEvent::Paired(sta, model) => {
                // rtlusb has turned WPS off already: one controller per pairing
                pair_until = None;
                // Named after its model the first time; a rename, or an earlier pairing, wins
                if !names.contains_key(&sta)
                    && let Some(name) = known_clients::clean(model)
                {
                    if let Err(e) = known_clients::save(&args.conf, &sta, &name) {
                        warn!("couldn't save the controller's name: {e:#}");
                    }
                    names.insert(sta, name);
                }
                info!(sta = %fmt_mac(&sta), ssid = group.ssid, "paired; waiting for the controller to rejoin");
            }
            RadioEvent::PairingFailed(sta, why) => {
                warn!(sta = %fmt_mac(&sta), "pairing failed: {why}; still listening, hold the button again")
            }
            RadioEvent::Tick => {
                if quit.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Relaxed);
                }
                if let Some(t) = pair_until
                    && now >= t
                {
                    pair_until = None;
                    link.set_wps(None)?;
                    warn!("no controller paired within {PAIR_WINDOW:?}; pairing is off");
                    sh.tray.problem(&tr!("problem-nothing-paired-title"), &tr!("problem-nothing-paired"));
                }
                if now.duration_since(last_slow) >= SLOW_POLL {
                    last_slow = now;
                    if let Some(t) = save_due
                        && now >= t
                    {
                        save_due = save_group(args, sh, &group, now, true);
                    }
                    for seat in &mut seats {
                        seat.poll(now, &mut desktop);
                    }
                    for seat in &mut seats {
                        let Some(why) = seat.pad_failed.take() else { continue };
                        warn!(sta = %fmt_mac(&seat.mac), "no virtual pad for it: {why}; sending it away");
                        sh.tray.problem(&tr!("problem-pad-title"), &tr!("problem-pad", error = why.as_str()));
                        if let Some(peer) = seat.host.peer() {
                            link.kick(&peer)?;
                            link.shun(&peer, SLEEP_SHUN);
                            seat.end_session(now, &mut desktop);
                        }
                    }
                    for cmd in sh.tray.commands() {
                        match cmd {
                            Command::Exit => {
                                quit.store(true, Ordering::Relaxed);
                                stop.store(true, Ordering::Relaxed);
                            }
                            Command::Pair if pair_until.is_none() => match wps_for(&group) {
                                Ok(w) => {
                                    link.set_wps(Some(w))?;
                                    pair_until = Some(now + PAIR_WINDOW);
                                    info!("pairing from the tray: hold the NVIDIA button until the light flashes");
                                }
                                Err(e) => sh.tray.problem(&tr!("problem-pair-title"), &format!("{e:#}")),
                            },
                            Command::Pair => {}
                            Command::Reconfigure => {
                                info!("reconfiguring the network from the tray; letting go of every controller");
                                next = Next::Reconfigure;
                                stop.store(true, Ordering::Relaxed);
                            }
                            Command::Vban(mac, on) => {
                                let on = sh.set_vban(&mac, on);
                                if let Some(s) = seat_mut(&mut seats, &mac) {
                                    s.set_vban(on, args.mic);
                                }
                            }
                            Command::Identify(mac) => {
                                if let Some(s) = seat_mut(&mut seats, &mac) {
                                    s.ctl.identify();
                                }
                            }
                            Command::Rename(mac, name) => {
                                if sh.rename(&mac, &name) {
                                    names.insert(mac, name);
                                }
                            }
                            Command::Disconnect(mac) => {
                                if let Some(s) = seat_mut(&mut seats, &mac)
                                    && s.leaving.is_none()
                                    && s.host.say_goodbye()
                                {
                                    info!(sta = %fmt_mac(&mac), "disconnecting from the tray; saying goodbye first");
                                    s.leaving = Some(Leaving::new(now));
                                }
                            }
                        }
                    }
                    for seat in &mut seats {
                        // Before the tray update, so it says "charge it" rather than "critical"
                        if seat.leaving.is_none() && seat.ctl.must_charge(now) && seat.host.say_goodbye() {
                            let sta = fmt_mac(&seat.mac);
                            warn!(%sta, "battery too low to use; sending the controller away until it charges");
                            seat.ctl.let_go();
                            seat.leaving = Some(Leaving::new(now));
                        }
                        // Never on the tick it started, so the tray has seen the flat battery and
                        // said so before the controller's gone
                        if let Some(l) = seat.leaving
                            && now > l.since
                            && (seat.host.all_delivered() || now >= l.deadline)
                        {
                            seat.leaving = None;
                            if let Some(peer) = seat.host.peer() {
                                let acked = seat.host.all_delivered();
                                info!(sta = %fmt_mac(&peer), acked, "goodbye done, kicking");
                                link.kick(&peer)?;
                                link.shun(&peer, SLEEP_SHUN);
                                seat.end_session(now, &mut desktop);
                            }
                        }
                        let present = seat.host.peer().is_some();
                        if seat.up() {
                            seat.joining = None;
                            seat.join_gave_up = false;
                        } else if seat.joining.is_some_and(|t| now.duration_since(t) >= JOIN_TIMEOUT) {
                            warn!(sta = %fmt_mac(&seat.mac), "controller hasn't come up in {JOIN_TIMEOUT:?}");
                            seat.joining = None;
                            seat.join_gave_up = present;
                        } else if present && seat.joining.is_none() && !seat.join_gave_up {
                            seat.joining = Some(now);
                        }
                        // A new session gets its own JOIN_TIMEOUT, Wi-Fi link or not
                        if !present {
                            seat.join_gave_up = false;
                        }
                        if let Some(t) = seat.last_rx
                            && now.duration_since(t) > STALE
                        {
                            debug!(sta = %fmt_mac(&seat.mac), "no frames for {STALE:?}, releasing outputs");
                            seat.release(&mut desktop);
                            seat.last_rx = None;
                        }
                    }
                    let controllers =
                        seats.iter_mut().filter_map(|s| s.connected(names.get(&s.mac).cloned())).collect();
                    let shunned = link
                        .shunned()
                        .into_iter()
                        .filter(|m| !seats.iter().any(|s| s.mac == *m && s.host.peer().is_some()))
                        .map(|mac| Shunned { mac, name: names.get(&mac).cloned() })
                        .collect();
                    sh.tray.update(Status {
                        controllers,
                        pairing: pair_until.is_some(),
                        ssid: Some(group.ssid.clone()),
                        channel: Some(channel),
                        joining: seats.iter().any(|s| s.joining.is_some()),
                        shunned,
                    });
                }
            }
        }
        for seat in &mut seats {
            let mic = seat.ctl.take_mic();
            if let Some(v) = sh.vban.as_mut()
                && seat.vban
                && !mic.is_empty()
            {
                v.push_mic(seat.mac, &mic, now);
            }
        }
        // Headset audio, one 4 ms unit per 4 ms of wall clock, the same to each that wants it
        match sh.vban.as_ref().filter(|_| seats.iter().any(Seat::wants_headset)) {
            Some(v) => {
                let at = isoc_at.replace(now).unwrap_or(now);
                isoc_credit = (isoc_credit + now.duration_since(at)).min(ISOC_CREDIT_MAX);
                let unit = Duration::from_millis(u64::from(vban::UNIT_MS));
                while isoc_credit >= unit {
                    isoc_credit -= unit;
                    if let Some(pcm) = v.take_unit() {
                        for seat in seats.iter_mut().filter(|s| s.wants_headset()) {
                            seat.send_headset(&pcm);
                        }
                    }
                }
            }
            None => {
                isoc_at = None;
                isoc_credit = Duration::ZERO;
            }
        }
        for seat in &mut seats {
            seat.rumble(now);
        }
        desktop.poll(now);
        for seat in &mut seats {
            seat.flush(link)?;
        }
        Ok(())
    })?;
    Ok(next)
}
