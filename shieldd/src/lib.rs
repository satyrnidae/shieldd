//! shieldd: hosts the 2015 SHIELD controller natively on Windows.
//!
//! Default: our own RTL8811AU driver (rtlusb over WinUSB) runs the AP ->
//! raw Ozmo frames -> an ozproto session per controller -> controller setup + HID reports ->
//! a virtual Xbox 360 pad each (ViGEm), plus a shared mouse and media keys (SendInput). See
//! radio.rs and seat.rs.
//!
//! The WSL bridge, for when the radio stays in Linux (ozwpan):
//! - `--transmit` (Linux): controller evdev/hidraw -> SHD2 UDP packets (transmit.rs)
//! - `--receiver` (Windows): SHD2 packets -> the same outputs (receiver.rs)
//!
//! Both sides share the packet format through the `shdlink` crate.
//!
//! Two binaries share this: `shieldd`, the console one, and `shieldw`, a windowless one for
//! running at login (`--install`) that lives in its own crate. Hosting, both log to a file too.
//! See background.rs, and shdlib's logs.rs.

#[cfg(windows)]
mod background;
#[cfg_attr(not(windows), allow(dead_code, reason = "only the Windows host uses it so far"))]
mod controller;
#[cfg_attr(not(windows), allow(dead_code, reason = "only the Windows host uses it so far"))]
mod known_clients;
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
#[cfg(windows)]
mod seat;
#[cfg(windows)]
mod signal;
#[cfg(target_os = "linux")]
mod transmit;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::error;

pub use shdlib::settings_dir;

/// For the tray's About box
#[cfg(windows)]
const ABOUT: shdtray::About = shdtray::About {
    version: env!("CARGO_PKG_VERSION"),
    authors: env!("CARGO_PKG_AUTHORS"),
    built: env!("SHIELDD_BUILD_DATE"),
};

pub fn default_conf() -> PathBuf {
    settings_dir().join("p2p.conf")
}

/// Whether `conf` is the default one, however it's spelled
#[cfg(windows)]
fn is_default_conf(conf: &std::path::Path) -> bool {
    match (std::fs::canonicalize(conf), std::fs::canonicalize(default_conf())) {
        (Ok(a), Ok(b)) => a == b,
        _ => conf == default_conf(),
    }
}

#[derive(Parser, Debug)]
#[command(version, about)]
#[allow(dead_code, reason = "each platform reads only its own options")]
pub struct Args {
    /// wpa_supplicant P2P config holding the controller's saved group
    #[arg(long, default_value_os_t = default_conf())]
    conf: PathBuf,
    /// Trackpad counts -> screen pixels. Sets X to this and Y to 1.5x it,
    /// since the pad's Y resolution is coarser.
    #[arg(long, default_value_t = 3.0)]
    mouse_scale: f32,
    /// Horizontal scale on its own, overriding --mouse-scale
    #[arg(long)]
    mouse_scale_x: Option<f32>,
    /// Vertical scale on its own, overriding --mouse-scale
    #[arg(long)]
    mouse_scale_y: Option<f32>,
    /// Open the mic stream even with VBAN off, where it goes nowhere. It keeps the controller from
    /// dozing between inputs, at the cost of a steady stream of frames. Doesn't change the lag.
    #[arg(long)]
    mic: bool,
    /// Leave the radio to WSL, where ozwpan runs the controller and --transmit sends its
    /// input, and only drive the outputs from those UDP packets
    #[arg(long)]
    receiver: bool,
    /// Pair a controller to the saved group: hold its NVIDIA button until the light flashes.
    /// Push-button stays on until one controller pairs, or 2 minutes; pair again for each more.
    /// Controllers already connected stay connected. Without a saved group, makes one first, as
    /// --reconfigure-network. Then hosts as usual.
    #[arg(long, conflicts_with_all = ["receiver", "transmit"])]
    pair: bool,
    /// Make a new group, a random DIRECT-xx-name on the quietest of channels 1, 6 and 11, and
    /// write it to --conf at once, keeping the old file gzipped in backup/ next to it. Then
    /// pairs as --pair. Every controller paired to the old group has to pair again.
    #[arg(long, conflicts_with_all = ["receiver", "transmit"])]
    reconfigure_network: bool,
    /// Where --transmit sends to, with --receiver
    #[arg(long, default_value = shdlink::DEFAULT_ADDR, requires = "receiver")]
    listen: SocketAddr,
    /// WSL side (Linux only): read the controller's evdev + hidraw nodes
    /// (ozwpan) and send SHD2 packets to a --receiver; replaces shield_tx.py
    #[arg(long, conflicts_with = "receiver")]
    transmit: bool,
    /// Where to send, with --transmit
    #[arg(long, default_value = shdlink::DEFAULT_ADDR, requires = "transmit")]
    dest: SocketAddr,
    /// Print raw events from every controller node instead of sending, with --transmit
    #[arg(long, requires = "transmit")]
    dump: bool,
    /// The controller's hidraw node (default: find 0955:7210), with --transmit
    #[arg(long, requires = "transmit")]
    hidraw: Option<PathBuf>,
    /// Keep running: wait for the adapter, and start over when it's unplugged or fails.
    /// shieldw always runs this way.
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair", "reconfigure_network"])]
    background: bool,
    /// Start shieldw at login for this user, with the rest of this command line. Doesn't start
    /// it now.
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair", "reconfigure_network", "uninstall", "stop"])]
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

/// Both binaries' main; `windowless` is shieldw, in its own crate, which only logs to the file.
/// See shdlib's logs.rs.
pub fn run(windowless: bool) -> Result<()> {
    let parsed = Args::try_parse();
    // A host takes the instance before it touches the log file, and only the instance starts a
    // new one. Anything else shieldw does, a second launch or a one-off like --stop, adds to
    // the end of it; shieldd's one-offs only print.
    #[cfg(windows)]
    let claimed = match &parsed {
        Ok(a) if a.hosts() => Some(background::claim(!windowless)),
        _ => None,
    };
    #[cfg(not(windows))]
    let claimed: Option<Result<Quit>> = None;
    let to_file = match &claimed {
        Some(Ok(_)) => Some(true),
        _ => windowless.then_some(false),
    };
    // rtlusb's info covers the AP, pairing and channel survey; its noisy stats are debug
    shdlib::logs::init(shdlib::logs::Setup {
        default_filter: "shieldd=info,rtlusb=info,shdtray=info,shdlib=info",
        console: !windowless,
        file: to_file.map(|rotate| (settings_dir().join("logs"), rotate)),
    })?;
    let claimed = match claimed.transpose() {
        Ok(c) => c,
        Err(e) => {
            if windowless {
                error!("{e:#}");
            }
            return Err(e);
        }
    };
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
                !(args.pair || args.reconfigure_network || args.receiver),
                "shieldw only hosts in the background; pair with shieldd or the tray"
            );
            args.background = true;
        }
        // run() claims for anything that hosts, before logging starts
        let quit = anyhow::Context::context(claimed, "hosting without the instance")?;
        if args.receiver {
            return receiver::run(args.listen, args.scale(), &quit);
        }
        radio::run(args, &quit)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
