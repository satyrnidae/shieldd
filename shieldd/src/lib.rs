//! shieldd: hosts the 2015 SHIELD controller natively on Windows.
//!
//! Default: our own RTL8811AU driver (rtlusb over WinUSB) runs the AP ->
//! raw Ozmo frames -> ozproto session -> controller setup + HID reports ->
//! virtual Xbox 360 pad (ViGEm), mouse and media keys (SendInput). See radio.rs.
//!
//! The WSL bridge, for when the radio stays in Linux (ozwpan):
//! - `--transmit` (Linux): controller evdev/hidraw -> SHD2 UDP packets (transmit.rs)
//! - `--receiver` (Windows): SHD2 packets -> the same outputs (receiver.rs)
//!
//! Both sides share the packet format through the `shdlink` crate.
//!
//! Two binaries share this: `shieldd`, the console one, and `shieldw`, a windowless one for
//! running at login (`--install`). Hosting, both log to a file too. See background.rs and logs.rs.

#[cfg(windows)]
mod background;
mod civil;
#[cfg(windows)]
mod controller;
#[cfg(windows)]
mod files;
#[cfg(windows)]
mod known_clients;
mod logs;
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
    /// Open the mic stream even w/ VBAN off, where it goes nowhere. It keeps the controller from
    /// dozing between inputs, at the cost of a steady stream of frames. Doesn't change the lag.
    #[arg(long)]
    mic: bool,
    /// Leave the radio to WSL, where ozwpan runs the controller and --transmit sends its
    /// input, and only drive the outputs from those UDP packets
    #[arg(long)]
    receiver: bool,
    /// Pair a controller: hold its NVIDIA button until the light flashes. Makes a new
    /// group (random DIRECT-xx-name, on the quietest of channels 1, 6 and 11) and writes it
    /// to --conf once the controller has it, keeping the old file as a .bak. Then hosts as
    /// usual.
    #[arg(long, conflicts_with_all = ["receiver", "transmit"])]
    pair: bool,
    /// Where --transmit sends to, w/ --receiver
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
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair"])]
    background: bool,
    /// Start shieldw at login for this user, w/ the rest of this command line. Doesn't start
    /// it now.
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair", "uninstall", "stop"])]
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

/// Both binaries' main; `windowless` is shieldw, which only logs to the file. See logs.rs.
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
    if init_logging(!windowless, to_file)? {
        // To the file; a console still gets Rust's own report, w/ the thread and any backtrace
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |p| {
            error!(target: PANIC, "{p}");
            if !windowless {
                report(p);
            }
        }));
    }
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

/// Panics, logged for the file; the console has Rust's own report of them
const PANIC: &str = "shieldd::panic";

/// `to_file`: Some(true) starts a new log file, Some(false) adds to it. A console carries on
/// w/o the file if it won't open. Color only goes to a terminal, never a file or a redirect.
/// Returns whether the file's in use.
fn init_logging(console: bool, to_file: Option<bool>) -> Result<bool> {
    use std::io::IsTerminal;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    // rtlusb's info covers the AP, pairing and channel survey; its noisy stats are debug
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "shieldd=info,rtlusb=info".into());
    let console = console.then(|| {
        fmt::layer()
            .with_ansi(std::io::stdout().is_terminal())
            .with_filter(tracing_subscriber::filter::filter_fn(|m| m.target() != PANIC))
    });
    // Nothing logs before the new file is in place, or it'd land in the last run's
    let (file, trouble) = match to_file.map(logs::open) {
        Some(Ok((f, stuck))) => (Some(fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f))), stuck),
        Some(Err(e)) if console.is_some() => (None, Some(e.context("logging to the console only"))),
        Some(Err(e)) => return Err(e),
        None => (None, None),
    };
    let to_file = to_file.filter(|_| file.is_some());
    tracing_subscriber::registry().with(filter).with(console).with(file).init();
    if let Some(e) = trouble {
        tracing::warn!("{e:#}");
    }
    if to_file == Some(true) {
        // Packing a long log takes a moment; hosting needn't wait on it
        let _ = std::thread::Builder::new().name("pack logs".into()).spawn(logs::pack);
    }
    Ok(to_file.is_some())
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
            anyhow::ensure!(!(args.pair || args.receiver), "shieldw only hosts in the background; pair w/ shieldd");
            args.background = true;
        }
        // run() claims for anything that hosts, before logging starts
        let quit = anyhow::Context::context(claimed, "hosting w/o the instance")?;
        if args.receiver {
            let mut outputs = outputs::Outputs::new(args.scale())?;
            return receiver::run(args.listen, &mut outputs, &quit);
        }
        radio::run(args, &quit)
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
