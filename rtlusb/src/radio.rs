//! The radio as one object: power up, run the AP, hand data frames to a callback that can
//! answer right away. Single-threaded on purpose: a reply goes out on the same thread that
//! received the trigger, with nothing queued in between.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nusb::transfer::{Bulk, In, TransferError};
use tracing::{debug, info, trace, warn};

use crate::ap::{ApConfig, WpsConfig};
use crate::bss::{Bss, Event, Mac};
use crate::chip::Chip;
use crate::tx::{Tx, TxInfo};
use crate::{efuse, phy, rx, survey, wpa};

const RX_BUF: usize = 32768;
const RX_TRANSFERS: usize = 8;
const TX_STATS_EVERY: Duration = Duration::from_secs(5);
/// LED blink half period while nobody's connected
const LED_BLINK: Duration = Duration::from_millis(500);
/// rx_loop gives up after this many failures in a row, bulk IN or callback. One success resets it.
const MAX_STRAIGHT_ERRORS: u32 = 50;

pub struct ApOptions {
    pub ssid: String,
    /// 0: survey first and take the quietest of 1, 6 and 11 (survey.rs)
    pub channel: u8,
    /// None: the adapter's own MAC
    pub bssid: Option<Mac>,
    pub pmk: wpa::Pmk,
    /// Hold data for a dozing station up to this long; None sends at once
    pub ps_hold: Option<Duration>,
    /// Keep the SSID out of beacons, see ApConfig::hidden
    pub hidden: bool,
    /// Pairing: WPS push-button until one enrollee has the credential
    pub wps: Option<WpsConfig>,
}

pub enum RadioEvent<'a> {
    /// See bss::Event::Associated
    Associated(Mac),
    Connected(Mac),
    Disconnected(Mac),
    Data {
        src: Mac,
        ethertype: u16,
        payload: &'a [u8],
    },
    /// WPS delivered the credential, w/ the model name the station gave; push-button is off again
    Paired(Mac, &'a str),
    PairingFailed(Mac, &'a str),
    /// Nothing received for a short while, or between batches of frames
    Tick,
}

/// Sending side, handed to the callback
pub struct Link<'a> {
    chip: &'a Chip,
    tx: &'a mut Tx,
    bss: &'a mut Bss,
    /// Events the callback caused, delivered once it returns
    deferred: &'a mut Vec<Event>,
}

impl Link<'_> {
    pub fn send(&mut self, dst: &Mac, ethertype: u16, payload: &[u8]) -> Result<()> {
        self.bss.send(self.chip, self.tx, dst, ethertype, payload)
    }

    /// See Bss::send_stream
    pub fn send_stream(&mut self, dst: &Mac, ethertype: u16, payload: &[u8]) -> Result<()> {
        self.bss.send_stream(self.chip, self.tx, dst, ethertype, payload)
    }

    /// Disconnects `sta`. Deauth only; see shieldd's goodbye for the Ozmo side. The callback
    /// gets the Disconnected right after it returns.
    pub fn kick(&mut self, sta: &Mac) -> Result<()> {
        self.deferred.extend(self.bss.kick(self.chip, self.tx, sta)?);
        Ok(())
    }

    /// See Bss::shun
    pub fn shun(&mut self, sta: &Mac, d: Duration) {
        self.bss.shun(sta, d)
    }
}

pub struct Radio {
    chip: Chip,
    tx: Tx,
    bss: Bss,
    deferred: Vec<Event>,
}

/// Power on and tune. The chip is powered off again if anything after power-on fails.
pub fn bring_up(channel: u8, promisc: bool) -> Result<Chip> {
    let chip = Chip::open()?;
    let start = Instant::now();
    chip.power_on().context("power on")?;
    info!(elapsed_ms = start.elapsed().as_millis() as u64, "powered on");
    let tuned = (|| {
        chip.set_mac(&chip.efuse.mac)?;
        chip.set_rx_filter(promisc)?;
        chip.set_rx_agg(false)?;
        chip.set_channel(channel)
    })();
    if let Err(e) = tuned {
        let _ = chip.power_off();
        return Err(e);
    }
    Ok(chip)
}

impl Radio {
    pub fn start(mut opts: ApOptions) -> Result<Self> {
        let auto = opts.channel == 0;
        let chip = bring_up(if auto { survey::CANDIDATES[0] } else { opts.channel }, auto)?;
        if auto {
            let picked = (|| {
                let c = survey::pick(&survey::survey(&chip)?);
                chip.set_rx_filter(false)?;
                chip.set_channel(c)?;
                Ok::<_, anyhow::Error>(c)
            })();
            match picked {
                Ok(c) => opts.channel = c,
                Err(e) => {
                    let _ = chip.power_off();
                    return Err(e.context("channel survey"));
                }
            }
        }
        match Self::start_ap(chip, opts) {
            Ok(r) => Ok(r),
            Err((chip, e)) => {
                let _ = chip.power_off();
                Err(e)
            }
        }
    }

    fn start_ap(chip: Chip, opts: ApOptions) -> Result<Self, (Chip, anyhow::Error)> {
        let setup = (|| {
            let bssid = opts.bssid.unwrap_or(chip.efuse.mac);
            let cfg = ApConfig {
                ssid: opts.ssid.clone(),
                bssid,
                channel: opts.channel,
                beacon_int: 100,
                hidden: opts.hidden,
                wps: opts.wps.clone(),
            };
            let (cck, ofdm, ht) = phy::set_tx_power_2g(&chip.rtl, &chip.efuse, opts.channel)?;
            info!(cck, ofdm, ht, "tx power index");
            let mut tx = Tx::new(&chip.rtl)?;
            chip.enable_sec_engine()?;
            chip.start_ap(&mut tx, &cfg)?;
            info!(ssid = cfg.ssid, bssid = efuse::fmt_mac(&bssid), channel = opts.channel, "AP up");
            let mut bss = Bss::new(cfg, opts.pmk);
            bss.set_ps_hold(opts.ps_hold);
            bss.install_gtk(&chip)?;
            Ok((tx, bss))
        })();
        match setup {
            Ok((tx, bss)) => Ok(Self { chip, tx, bss, deferred: Vec::new() }),
            Err(e) => Err((chip, e)),
        }
    }

    pub fn bssid(&self) -> Mac {
        self.bss.cfg.bssid
    }

    /// The channel hosting, after any survey
    pub fn channel(&self) -> u8 {
        self.bss.cfg.channel
    }

    /// Runs until `quit` or an error. Probe requests, auth, association and EAPOL are handled
    /// here; the callback sees connections, data frames and ticks.
    pub fn run(&mut self, quit: &AtomicBool, f: &mut dyn FnMut(RadioEvent, &mut Link) -> Result<()>) -> Result<()> {
        let Radio { chip, tx, bss, deferred } = self;
        let mut last_stats = Instant::now();
        let t0 = Instant::now();
        let mut led: Option<bool> = None;
        rx_loop(chip, None, Duration::from_millis(10), quit, &mut |p| {
            // Solid when connected, blinking while waiting
            let want = bss.any_connected() || (t0.elapsed().as_millis() / LED_BLINK.as_millis()) % 2 == 0;
            if led != Some(want) {
                led = Some(want);
                chip.set_led(want)?;
            }
            if last_stats.elapsed() >= TX_STATS_EVERY {
                last_stats = Instant::now();
                let ps = bss.take_ps_stats();
                let (direct, on_wake, timed_out, max_wait_ms, dropped, stream) =
                    (ps.direct, ps.on_wake, ps.timed_out, ps.max_wait_ms, ps.dropped, ps.stream);
                if dropped > 0 {
                    debug!(direct, dropped, "downlink backed up, frames dropped, last 5 s");
                }
                debug!(direct, on_wake, timed_out, max_wait_ms, stream, "power save, last 5 s");
                let s = tx.take_stats();
                let (acked, failed, unreported, retries, max_retries) =
                    (s.acked, s.failed, s.unreported, s.retries, s.max_retries);
                // ~1% failed is normal on a busy channel; worth a mention past 5%
                let sent = acked + failed + unreported;
                if (failed + unreported) * 20 > sent {
                    info!(acked, failed, unreported, retries, max_retries, "tx reports, last 5 s");
                } else {
                    debug!(acked, retries, max_retries, "tx reports, last 5 s");
                }
            }
            if let Some(p) = p
                && p.c2h
            {
                if !tx.on_c2h(p.frame) {
                    trace!(c2h = ?p.frame, "C2H");
                }
                return Ok(());
            }
            let Some(p) = p else {
                for e in bss.tick(chip, tx)? {
                    deliver(e, chip, tx, bss, deferred, f)?;
                }
                f(RadioEvent::Tick, &mut Link { chip, tx, bss, deferred })?;
                return flush(chip, tx, bss, deferred, f);
            };
            let frame = p.frame;
            if frame.len() >= 16 {
                let fc = u16::from_le_bytes([frame[0], frame[1]]);
                let sa: Mac = frame[10..16].try_into().unwrap();
                if rx::frame_kind(fc) == "probe-req" && bss.cfg.wants(frame) && !bss.is_shunned(&sa) {
                    tx.send(&chip.rtl, TxInfo::mgmt(), &bss.cfg.probe_resp(&sa))?;
                }
            }
            if let Some(e) = bss.handle(chip, tx, frame, p.decrypted)? {
                deliver(e, chip, tx, bss, deferred, f)?;
            }
            Ok(())
        })
    }
}

type Callback<'f> = dyn FnMut(RadioEvent, &mut Link) -> Result<()> + 'f;

/// Hands `e` to the callback, then whatever that caused
fn deliver(
    e: Event,
    chip: &Chip,
    tx: &mut Tx,
    bss: &mut Bss,
    deferred: &mut Vec<Event>,
    f: &mut Callback,
) -> Result<()> {
    deliver_one(e, chip, tx, bss, deferred, f)?;
    flush(chip, tx, bss, deferred, f)
}

fn flush(chip: &Chip, tx: &mut Tx, bss: &mut Bss, deferred: &mut Vec<Event>, f: &mut Callback) -> Result<()> {
    while !deferred.is_empty() {
        let e = deferred.remove(0);
        deliver_one(e, chip, tx, bss, deferred, f)?;
    }
    Ok(())
}

fn deliver_one(
    e: Event,
    chip: &Chip,
    tx: &mut Tx,
    bss: &mut Bss,
    deferred: &mut Vec<Event>,
    f: &mut Callback,
) -> Result<()> {
    if let Event::Paired(..) = e {
        // One controller per pairing: drop the WPS IE, and hide the SSID again if asked to
        bss.cfg.wps = None;
        chip.download_beacon(tx, &bss.cfg.beacon())?;
    }
    let ev = match &e {
        Event::Associated(m) => RadioEvent::Associated(*m),
        Event::Connected(m) => RadioEvent::Connected(*m),
        Event::Disconnected(m) => RadioEvent::Disconnected(*m),
        Event::Data(src, ethertype, payload) => RadioEvent::Data { src: *src, ethertype: *ethertype, payload },
        Event::Paired(m, model) => RadioEvent::Paired(*m, model),
        Event::PairingFailed(m, why) => RadioEvent::PairingFailed(*m, why),
    };
    f(ev, &mut Link { chip, tx, bss, deferred })
}

impl Drop for Radio {
    fn drop(&mut self) {
        info!("powering off");
        let _ = self.chip.set_led(false);
        if let Err(e) = self.chip.power_off() {
            warn!("power off: {e:#}");
        }
    }
}

/// Bulk IN loop. `on_frame(None)` runs every pass, at least every `idle`. C2H packets from the
/// firmware come through too, w/ `c2h` set.
pub fn rx_loop(
    chip: &Chip,
    limit: Option<Duration>,
    idle: Duration,
    quit: &AtomicBool,
    on_frame: &mut dyn FnMut(Option<&rx::RxPkt>) -> Result<()>,
) -> Result<()> {
    let mut ep = chip.rtl.interface().endpoint::<Bulk, In>(chip.rtl.bulk_in)?;
    for _ in 0..RX_TRANSFERS {
        let buf = ep.allocate(RX_BUF);
        ep.submit(buf);
    }

    let t0 = Instant::now();
    let mut last_report = Instant::now();
    let (mut frames, mut crc_errs, mut c2h) = (0u64, 0u64, 0u64);
    let mut straight = 0u32;
    // A failure is logged and survived, unless the adapter's gone or nothing's working anymore
    let mut check = |r: Result<()>| -> Result<()> {
        let Err(e) = r else {
            straight = 0;
            return Ok(());
        };
        if gone(&e) {
            return Err(e.context("adapter unplugged"));
        }
        straight += 1;
        if straight >= MAX_STRAIGHT_ERRORS {
            return Err(e.context(format!("{straight} failures in a row")));
        }
        warn!("{e:#}");
        Ok(())
    };
    let res = (|| {
        while !quit.load(Ordering::SeqCst) && limit.is_none_or(|l| t0.elapsed() < l) {
            if last_report.elapsed() >= Duration::from_secs(5) {
                tracing::debug!(frames, crc_errs, c2h, "rx stats, last 5 s");
                (frames, crc_errs, c2h) = (0, 0, 0);
                last_report = Instant::now();
            }
            check(on_frame(None))?;
            let Some(done) = ep.wait_next_complete(idle) else {
                continue;
            };
            check(done.status.map_err(|e| anyhow::Error::new(e).context("bulk IN")))?;
            let mut buf = done.buffer;
            for p in rx::split(&buf[..done.actual_len.min(buf.len())]) {
                if p.c2h {
                    c2h += 1;
                }
                if p.crc_err {
                    crc_errs += 1;
                    continue;
                }
                frames += 1;
                check(on_frame(Some(&p)))?;
            }
            buf.clear();
            buf.set_requested_len(RX_BUF);
            ep.submit(buf);
        }
        Ok(())
    })();
    ep.cancel_all();
    res
}

/// Whether `e` came from the adapter going away
fn gone(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(c.downcast_ref::<TransferError>(), Some(TransferError::Disconnected))
            || c.downcast_ref::<nusb::Error>().is_some_and(|e| e.kind() == nusb::ErrorKind::Disconnected)
    })
}
