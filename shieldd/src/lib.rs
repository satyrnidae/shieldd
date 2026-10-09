//! shieldd: hosts the 2015 SHIELD controller natively on Windows.
//!
//! Default: our own RTL8811AU driver (rtlusb over WinUSB) runs the AP ->
//! raw Ozmo frames -> ozproto session -> controller setup + HID reports ->
//! virtual Xbox 360 pad (ViGEm), mouse and media keys (SendInput). See radio.rs.
//!
//! `--wifi-direct`: the older path on the Realtek Windows driver, Wi-Fi Direct
//! GO (WinRT) + Npcap. Works, w/ ~330 ms of input lag. See host.rs.
//!
//! The WSL bridge, for when the radio stays in Linux (ozwpan):
//! - `--transmit` (Linux): controller evdev/hidraw -> SHD2 UDP packets (transmit.rs)
//! - `--receiver` (Windows): SHD2 packets -> the same outputs (receiver.rs)
//!
//! Both sides share the packet format through the `shdlink` crate.
//!
//! Two binaries share this: `shieldd`, the console one, and `shieldw`, a windowless one for
//! running at login (`--install`), which logs to a file. See background.rs.

#[cfg(windows)]
mod adapters;
#[cfg(windows)]
mod background;
#[cfg(windows)]
mod controller;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod models;
#[cfg(windows)]
mod npcap;
#[cfg(windows)]
mod outputs;
#[cfg(windows)]
mod p2pconf;
#[cfg(windows)]
mod pair;
#[cfg(windows)]
mod radio;
#[cfg(windows)]
mod receiver;
#[cfg(target_os = "linux")]
mod transmit;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod vban;
#[cfg(windows)]
mod wifi;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::error;

/// The user's local settings folder: `%LOCALAPPDATA%\shieldd` on Windows, local rather than
/// roaming since the group is tied to this machine's adapter, and `$XDG_CONFIG_HOME/shieldd`
/// or `~/.config/shieldd` elsewhere. The working directory if neither is set.
pub fn settings_dir() -> PathBuf {
    use std::env::var_os;
    #[cfg(windows)]
    let dir = var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let dir = var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    dir.map_or_else(PathBuf::new, |d| d.join("shieldd"))
}

pub fn default_conf() -> PathBuf {
    settings_dir().join("p2p.conf")
}

#[derive(Parser, Debug)]
#[command(version, about)]
#[allow(dead_code, reason = "each platform reads only its own options")]
pub struct Args {
    /// wpa_supplicant P2P config holding the controller's saved group
    #[arg(long, default_value_os_t = default_conf())]
    conf: PathBuf,
    /// Adapter (and radio) that hosts the group, by its Windows name, w/ --wifi-direct
    #[arg(long, default_value = "Wi-Fi 2")]
    adapter: String,
    /// Connect the adapter to this saved network first, to pin the GO's channel
    #[arg(long, requires = "wifi_direct")]
    anchor_ssid: Option<String>,
    /// Frequency of the anchor network; the controller only searches channel 1
    #[arg(long, default_value_t = 2412)]
    anchor_mhz: u32,
    /// Trackpad counts -> screen pixels. Sets X to this and Y to 1.5x it,
    /// since the pad's Y resolution is coarser.
    #[arg(long, default_value_t = 6.0)]
    mouse_scale: f32,
    /// Horizontal scale on its own, overriding --mouse-scale
    #[arg(long)]
    mouse_scale_x: Option<f32>,
    /// Vertical scale on its own, overriding --mouse-scale
    #[arg(long)]
    mouse_scale_y: Option<f32>,
    /// Open the mic stream. Nothing reads it yet; it keeps the controller from dozing between
    /// inputs, at the cost of a steady stream of frames. Doesn't change the lag.
    #[arg(long)]
    mic: bool,
    /// Leave the radio to WSL (ozwpan + shield_tx.py) and only drive the
    /// outputs from its UDP packets, like shield_rx.py
    #[arg(long)]
    receiver: bool,
    /// Old host path: Windows Wi-Fi Direct + Npcap on the Realtek driver,
    /// ~330 ms of input lag, since the stack holds frames for the dozing controller. The
    /// default drives the RTL8811AU ourselves (rtlusb), which needs the adapter on WinUSB.
    #[arg(long, conflicts_with = "receiver")]
    wifi_direct: bool,
    /// Pair a controller: hold its NVIDIA button until the light flashes. Makes a new
    /// group (random DIRECT-xx-name, on the quietest of channels 1, 6 and 11) and writes it
    /// to --conf once the controller has it, keeping the old file as a .bak. Then hosts as
    /// usual.
    #[arg(long, conflicts_with_all = ["receiver", "wifi_direct", "transmit"])]
    pair: bool,
    /// Where shield_tx.py sends to, w/ --receiver
    #[arg(long, default_value = shdlink::DEFAULT_ADDR, requires = "receiver")]
    listen: SocketAddr,
    /// WSL side (Linux only): read the controller's evdev + hidraw nodes
    /// (ozwpan) and send SHD2 packets to a --receiver; replaces shield_tx.py
    #[arg(long, conflicts_with = "receiver")]
    transmit: bool,
    /// Where to send, w/ --transmit
    #[arg(long, default_value = shdlink::DEFAULT_ADDR, requires = "transmit")]
    dest: SocketAddr,
    /// Print raw events from every controller node instead of sending, w/ --transmit
    #[arg(long, requires = "transmit")]
    dump: bool,
    /// The controller's hidraw node (default: find 0955:7210), w/ --transmit
    #[arg(long, requires = "transmit")]
    hidraw: Option<PathBuf>,
    /// Keep running: wait for the adapter, and start over when it's unplugged or fails.
    /// shieldw always runs this way.
    #[arg(long, conflicts_with_all = ["receiver", "wifi_direct", "transmit", "pair"])]
    background: bool,
    /// Start shieldw at login for this user, w/ the rest of this command line. Doesn't start
    /// it now.
    #[arg(long, conflicts_with_all = ["receiver", "wifi_direct", "transmit", "pair", "uninstall", "stop"])]
    install: bool,
    /// Don't start shieldw at login anymore. Doesn't stop it either; see --stop.
    #[arg(long, exclusive = true)]
    uninstall: bool,
    /// Stop the shieldd or shieldw that's running now
    #[arg(long, exclusive = true)]
    stop: bool,
}

#[cfg(windows)]
impl Args {
    /// Not a one-off like --install or --stop
    fn hosts(&self) -> bool {
        !(self.install || self.uninstall || self.stop || self.transmit)
    }

    fn scale(&self) -> (f32, f32) {
        (self.mouse_scale_x.unwrap_or(self.mouse_scale), self.mouse_scale_y.unwrap_or(self.mouse_scale * 1.5))
    }
}

/// From background::claim
type Quit = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// Both binaries' main; `windowless` is shieldw, which logs to `settings_dir()/shieldd.log`
pub fn run(windowless: bool) -> Result<()> {
    let parsed = Args::try_parse();
    // shieldw takes the instance before its log rotates, so a second launch quits w/o touching
    // the running one's log
    #[cfg(windows)]
    let claimed = match &parsed {
        Ok(a) if windowless && a.hosts() => Some(background::claim(false)?),
        _ => None,
    };
    #[cfg(not(windows))]
    let claimed = None;
    init_logging(windowless)?;
    if windowless {
        std::panic::set_hook(Box::new(|p| error!("{p}")));
    }
    let args = match parsed {
        Ok(a) => a,
        Err(e) if windowless => {
            error!("{e}");
            return Err(e.into());
        }
        Err(e) => e.exit(),
    };
    let result = dispatch(args, windowless, claimed);
    if windowless && let Err(e) = &result {
        error!("{e:#}");
    }
    result
}

fn init_logging(to_file: bool) -> Result<()> {
    // rtlusb's info covers the AP, pairing and channel survey; its noisy stats are debug
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "shieldd=info,rtlusb=info".into());
    let fmt = tracing_subscriber::fmt().with_env_filter(filter);
    if to_file {
        let dir = settings_dir();
        std::fs::create_dir_all(&dir)?;
        let log = dir.join("shieldd.log");
        // One run back is enough to see why it stopped
        let _ = std::fs::rename(&log, dir.join("shieldd.old.log"));
        let file = std::fs::File::create(log)?;
        fmt.with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
    } else {
        fmt.init();
    }
    Ok(())
}

#[cfg_attr(not(windows), expect(unused_mut, reason = "only Windows sets background"))]
fn dispatch(mut args: Args, windowless: bool, claimed: Option<Quit>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = (windowless, claimed);
        anyhow::ensure!(args.transmit, "on Linux only --transmit is supported (the WSL side of the bridge)");
        transmit::run(args.dest, args.hidraw.as_deref(), args.dump)
    }
    #[cfg(windows)]
    {
        anyhow::ensure!(!args.transmit, "--transmit is the WSL side; run it in Linux");
        if args.install {
            return background::install(&args);
        }
        if args.uninstall {
            return background::uninstall();
        }
        if args.stop {
            return background::stop();
        }
        if windowless {
            anyhow::ensure!(
                !(args.pair || args.receiver || args.wifi_direct),
                "shieldw only hosts in the background; pair w/ shieldd"
            );
            args.background = true;
        }
        let quit = match claimed {
            Some(q) => q,
            None => background::claim(!windowless)?,
        };
        host::run(args, quit)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
