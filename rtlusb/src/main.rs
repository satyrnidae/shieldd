//! rtlusb CLI: efuse dump, receive-only monitor and a standalone AP for testing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use shdnet::mac::{Mac, MacFmt};
use tracing::{info, warn};

use rtlusb::chip::Chip;
use rtlusb::radio::{bring_up, rx_loop};
use rtlusb::regs::*;
use rtlusb::{ap, bss, efuse, phy, rx, tx, usb, wpa};

/// Where shieldd keeps the group (shieldd main.rs)
fn default_conf() -> PathBuf {
    use std::env::var_os;
    #[cfg(target_os = "windows")]
    let dir = var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let dir = var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    dir.map_or_else(PathBuf::new, |d| d.join("shieldd")).join("p2p.conf")
}

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// M0: chip version and efuse, no power-up
    Efuse {
        /// Write the physical and logical maps to <dir>/efuse-{phys,logical}.bin
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// M1: power up, load firmware, tune and print received frames until Ctrl-C
    Rx {
        #[arg(long, default_value_t = 1)]
        channel: u8,
        /// Only print frames to or from this MAC
        #[arg(long)]
        mac: Option<String>,
        /// Hide beacons
        #[arg(long)]
        no_beacons: bool,
        /// Only RCR defaults: our MAC, broadcast, multicast. Default accepts everything.
        #[arg(long)]
        no_promisc: bool,
        /// Stop after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// M2: beacon as an AP and answer probe requests, printing what the controller sends
    Ap {
        #[arg(long, default_value = "DIRECT-0S")]
        ssid: String,
        #[arg(long, default_value_t = 1)]
        channel: u8,
        /// Defaults to the adapter's own MAC, which is what the Linux group owner used
        #[arg(long)]
        bssid: Option<String>,
        /// Also print frames that aren't to or from us
        #[arg(long)]
        all: bool,
        /// wpa_supplicant network block holding the group's psk=
        #[arg(long, default_value_os_t = default_conf())]
        psk_conf: PathBuf,
        /// Stop after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
}

fn main() -> Result<()> {
    // Before the log, so --help and a typo don't start a new one
    let args = Args::parse();
    shdlib::logs::init(shdlib::logs::Setup {
        default_filter: "rtlusb=info,shdlib=info",
        console: true,
        file: Some((shdlib::settings_dir().join("logs").join("rtlusb"), true)),
    })?;
    match args.cmd {
        Cmd::Efuse { dump } => efuse_cmd(dump),
        Cmd::Rx { channel, mac, no_beacons, no_promisc, seconds } => {
            rx_cmd(channel, mac, no_beacons, !no_promisc, seconds.map(Duration::from_secs))
        }
        Cmd::Ap { ssid, channel, bssid, all, psk_conf, seconds } => {
            ap_cmd(ssid, channel, bssid, all, &psk_conf, seconds.map(Duration::from_secs))
        }
    }
}

fn efuse_cmd(dump: Option<PathBuf>) -> Result<()> {
    let rtl = usb::Rtl::open()?;
    let sys_cfg = rtl.r32(SYS_CFG1)?;
    let eeprom = rtl.r8(SYS_EEPROM_CTRL)?;
    println!("SYS_CFG {sys_cfg:#010x}, chip cut {}", (sys_cfg >> 12) & 0xf);
    println!("EEPROM_CTRL {eeprom:#04x}, autoload {}", if eeprom & EEPROM_AUTOLOAD_OK != 0 { "ok" } else { "FAILED" });

    let phys = efuse::read_phys(&rtl)?;
    let used = phys.iter().rposition(|&b| b != 0xff).map_or(0, |i| i + 1);
    println!("physical efuse: {used} of {} bytes used", efuse::PHYS_SIZE);
    let log = efuse::logical(&phys);
    println!("logical efuse:");
    efuse::hexdump(&log);
    let e = efuse::Efuse::parse(&log);
    println!("efuse VID:PID {:04x}:{:04x}", e.vid, e.pid);
    println!("MAC {}", e.mac.fmt());

    if let Some(dir) = dump {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("efuse-phys.bin"), &phys)?;
        std::fs::write(dir.join("efuse-logical.bin"), &log)?;
        println!("wrote {}", dir.display());
    }
    Ok(())
}

fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let bytes: Vec<u8> = s
        .split([':', '-'])
        .map(|p| u8::from_str_radix(p, 16))
        .collect::<Result<_, _>>()
        .with_context(|| format!("bad MAC {s}"))?;
    bytes.try_into().map_err(|_| anyhow::anyhow!("bad MAC {s}"))
}

fn quit_flag() -> Result<Arc<AtomicBool>> {
    let quit = Arc::new(AtomicBool::new(false));
    let q = quit.clone();
    ctrlc::set_handler(move || q.store(true, Ordering::SeqCst))?;
    Ok(quit)
}

fn power_off(chip: &Chip) {
    info!("powering off");
    if let Err(e) = chip.power_off() {
        warn!("power off: {e:#}");
    }
}

fn rx_cmd(channel: u8, mac: Option<String>, no_beacons: bool, promisc: bool, limit: Option<Duration>) -> Result<()> {
    let filter = mac.as_deref().map(parse_mac).transpose()?;
    let quit = quit_flag()?;
    let chip = bring_up(channel, promisc)?;
    info!(channel, promisc, "receiving, Ctrl-C to stop");
    let t0 = Instant::now();
    let res = rx_loop(&chip, limit, Duration::from_millis(50), &quit, &mut |p| {
        let Some(p) = p.filter(|p| !p.c2h) else {
            return Ok(());
        };
        if !(no_beacons && is_beacon(p)) && matches(p, filter) {
            print_frame(p, t0);
        }
        Ok(())
    });
    power_off(&chip);
    res
}

fn ap_cmd(
    ssid: String,
    channel: u8,
    bssid: Option<String>,
    all: bool,
    psk_conf: &std::path::Path,
    limit: Option<Duration>,
) -> Result<()> {
    let conf = std::fs::read_to_string(psk_conf).with_context(|| format!("reading {}", psk_conf.display()))?;
    let pmk = wpa::pmk_from_conf(&conf, &ssid)?;
    let quit = quit_flag()?;
    let chip = bring_up(channel, true)?;
    let res = (|| {
        let bssid = match bssid {
            Some(b) => parse_mac(&b)?,
            None => chip.efuse.mac,
        };
        let cfg = ap::ApConfig { ssid, bssid, channel, beacon_int: 100, hidden: false, wps: None };
        let (cck, ofdm, ht) = phy::set_tx_power_2g(&chip.rtl, &chip.efuse, channel)?;
        info!(cck, ofdm, ht, "tx power index");
        let mut tx = tx::Tx::new(&chip.rtl)?;
        chip.enable_sec_engine()?;
        chip.start_ap(&mut tx, &cfg)?;
        info!(ssid = cfg.ssid, bssid = bssid.fmt(), channel, "beaconing, Ctrl-C to stop");
        let mut bss = bss::Bss::new(cfg, pmk);
        bss.install_gtk(&chip)?;

        let t0 = Instant::now();
        let mut ozmo_frames = 0u64;
        rx_loop(&chip, limit, Duration::from_millis(50), &quit, &mut |p| {
            let Some(p) = p else {
                for e in bss.tick(&chip, &mut tx)? {
                    info!(?e, "bss");
                }
                return Ok(());
            };
            if p.c2h {
                tx.on_c2h(p.frame);
                // For Bss::note_tx and the signal bars; nothing here wants them
                tx.take_reports();
                return Ok(());
            }
            let f = p.frame;
            if f.len() < 10 {
                return Ok(());
            }
            let fc = u16::from_le_bytes([f[0], f[1]]);
            let kind = rx::frame_kind(fc);
            let ours = f[4..10] == bssid || f.get(16..22) == Some(&bssid[..]);
            let probe_for_us = kind == "probe-req" && f.len() >= 16 && bss.cfg.wants(f);
            let quiet = matches!(kind, "ack" | "qos-data" | "data" | "qos-null" | "null");
            if all || ((ours || probe_for_us) && !quiet) {
                print_frame(p, t0);
            }
            if probe_for_us {
                let sa: [u8; 6] = f[10..16].try_into().unwrap();
                tx.send(&chip.rtl, tx::TxInfo::mgmt(), &bss.cfg.probe_resp(&sa))?;
            }
            match bss.handle(&chip, &mut tx, f, p.decrypted)? {
                Some(bss::Event::Data(sta, ethertype, payload)) => {
                    ozmo_frames += 1;
                    if ozmo_frames <= 20 || ozmo_frames.is_multiple_of(500) {
                        info!(
                            sta = sta.fmt(),
                            ethertype = format!("{ethertype:#06x}"),
                            len = payload.len(),
                            head = format!("{:02x?}", &payload[..payload.len().min(16)]),
                            n = ozmo_frames,
                            "data"
                        );
                    }
                }
                Some(e) => info!(?e, "bss"),
                None => {}
            }
            Ok(())
        })
    })();
    power_off(&chip);
    res
}

fn is_beacon(p: &rx::RxPkt) -> bool {
    p.frame.len() >= 2 && rx::frame_kind(u16::from_le_bytes([p.frame[0], p.frame[1]])) == "beacon"
}

/// Frame is to or from `filter`, or there's no filter
fn matches(p: &rx::RxPkt, filter: Option<[u8; 6]>) -> bool {
    let Some(m) = filter else { return true };
    p.frame.get(4..10) == Some(&m[..]) || p.frame.get(10..16) == Some(&m[..])
}

fn print_frame(p: &rx::RxPkt, t0: Instant) {
    let f = p.frame;
    if f.len() < 10 {
        return;
    }
    let fc = u16::from_le_bytes([f[0], f[1]]);
    let kind = rx::frame_kind(fc);
    let a1: Mac = f[4..10].try_into().unwrap();
    let a2 = f.get(10..16).and_then(|a| Mac::try_from(a).ok());
    let flags: String = [(0x1000, 'P'), (0x0800, 'R'), (0x4000, 'W')]
        .iter()
        .filter(|(bit, _)| fc & bit != 0)
        .map(|(_, c)| *c)
        .collect();
    let mut line = format!(
        "{:9.3} {:>5} {:>4} {:<13} {} <- {}",
        t0.elapsed().as_secs_f64(),
        rx::rate_name(p.rate),
        p.dbm.map_or("?".into(), |d| d.to_string()),
        kind,
        a1.fmt(),
        a2.map_or("-".into(), |a| a.fmt()),
    );
    if !flags.is_empty() {
        line += &format!(" [{flags}]");
    }
    if let Some(s) = rx::ssid(f) {
        line += &format!(" ssid={s:?}");
    }
    println!("{line}");
}
