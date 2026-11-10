//! shieldd: hosts the 2015 SHIELD controller natively, on Windows or Linux.
//!
//! Default: our own RTL8811AU driver (rtlusb over WinUSB or usbfs) runs the AP ->
//! raw Ozmo frames -> an ozproto session per controller -> controller setup + HID reports ->
//! a virtual Xbox 360 pad each, plus a shared mouse and media keys: ViGEm and SendInput on
//! Windows, uinput on Linux. See radio.rs, seat.rs and outputs.rs.
//!
//! The WSL bridge, for when the radio stays in Linux (ozwpan):
//! - `--transmit` (Linux): controller evdev/hidraw -> SHD2 UDP packets (transmit.rs)
//! - `--receiver` (Windows, usually): SHD2 packets -> the same outputs (receiver.rs)
//!
//! Both sides share the packet format through the `shdlink` crate.
//!
//! Two binaries share this on Windows: `shieldd`, the console one, and `shieldw`, a windowless one
//! for running at login (`--install`) that lives in its own crate. Linux has no console to hide
//! and starts `shieldd --background` instead. Hosting, both log to a file too.
//! See background.rs, and shdlib's logs.rs.

mod background;
mod controller;
mod known_clients;
mod outputs;
mod p2pconf;
mod pair;
mod radio;
mod receiver;
mod seat;
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
const ABOUT: shdtray::About = shdtray::About {
    version: env!("CARGO_PKG_VERSION"),
    authors: env!("CARGO_PKG_AUTHORS"),
    built: env!("SHIELDD_BUILD_DATE"),
    source: env!("CARGO_PKG_REPOSITORY"),
};

pub fn default_conf() -> PathBuf {
    settings_dir().join("p2p.conf")
}

/// Whether `conf` is the default one, however it's spelled
fn is_default_conf(conf: &std::path::Path) -> bool {
    match (std::fs::canonicalize(conf), std::fs::canonicalize(default_conf())) {
        (Ok(a), Ok(b)) => a == b,
        _ => conf == default_conf(),
    }
}

#[derive(Parser, Debug)]
#[command(version, about)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "--transmit's options are read on Linux only"))]
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
    /// Trackpad gain for the slowest finger motion, as a fraction of --mouse-scale. It ramps up to
    /// the full scale as the finger speeds up, so small moves land on exact pixels. 1 turns
    /// acceleration off.
    #[arg(long, default_value_t = 0.35, value_parser = non_negative)]
    mouse_accel_min: f32,
    /// Finger speed, in trackpad counts per ms, where the gain reaches the full --mouse-scale
    #[arg(long, default_value_t = 1.0, value_parser = non_negative)]
    mouse_accel_knee: f32,
    /// Trackball emulation: flick the trackpad and let go, and the pointer keeps gliding and slows
    /// to a stop. Needs acceleration, so it can't go with --mouse-accel-min 1. Not for --receiver.
    #[arg(long)]
    trackball: bool,
    /// How long the --trackball glide takes to lose about two thirds of its speed, in ms. Longer
    /// glides further.
    #[arg(long, default_value_t = 350.0, value_parser = positive)]
    trackball_glide: f32,
    /// Open the mic stream even with audio off, where it goes nowhere. It keeps the controller from
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
    /// Where controller audio goes: `vban`, streams for Voicemeeter, or on Linux `pipewire`, a
    /// mic and a headset in the sound settings. By default PipeWire where it's running, VBAN
    /// otherwise.
    #[arg(long, value_enum, conflicts_with_all = ["receiver", "transmit"])]
    audio_backend: Option<AudioBackend>,
    /// No tray icon: no menu, boxes or sounds, and its notifications go to the log. For hosting
    /// without a desktop to show it on.
    #[arg(long, conflicts_with_all = ["receiver", "transmit"])]
    no_tray: bool,
    /// Keep running: wait for the adapter, and start over when it's unplugged or fails.
    /// shieldw always runs this way, as does the login start on Linux.
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair", "reconfigure_network"])]
    background: bool,
    /// Start at login for this user, with the rest of this command line: shieldw on Windows,
    /// `shieldd --background` on Linux. Doesn't start it now. On Linux it also puts in the udev
    /// rule (see --install-udev-rule), so run it with sudo; it's set up for the user who ran sudo.
    #[arg(long, conflicts_with_all = ["receiver", "transmit", "pair", "reconfigure_network", "uninstall", "stop"])]
    install: bool,
    /// Don't start at login anymore. Doesn't stop it either; see --stop. On Linux it also
    /// takes out the udev rule, so run it with sudo.
    #[arg(long, exclusive = true)]
    uninstall: bool,
    /// Let the user logged in at the seat drive the adapter and the virtual pads without root:
    /// shieldd's udev rule in /etc/udev/rules.d, and uinput loaded at boot. Run it with sudo.
    /// --install does this too.
    #[cfg(target_os = "linux")]
    #[arg(long, exclusive = true)]
    install_udev_rule: bool,
    /// Stop the shieldd or shieldw that's running now
    #[arg(long, exclusive = true)]
    stop: bool,
}

impl Args {
    /// Not a one-off like --install or --stop
    fn hosts(&self) -> bool {
        !(self.install || self.uninstall || self.stop || self.transmit || self.installs_udev_rule())
    }

    #[cfg(target_os = "linux")]
    fn installs_udev_rule(&self) -> bool {
        self.install_udev_rule
    }

    #[cfg(not(target_os = "linux"))]
    fn installs_udev_rule(&self) -> bool {
        false
    }

    fn scale(&self) -> (f32, f32) {
        (self.mouse_scale_x.unwrap_or(self.mouse_scale), self.mouse_scale_y.unwrap_or(self.mouse_scale * 1.5))
    }

    fn accel(&self) -> outputs::Accel {
        outputs::Accel { min: self.mouse_accel_min, knee: self.mouse_accel_knee }
    }

    /// The glide, if --trackball asked for one
    fn trackball(&self) -> Option<outputs::Trackball> {
        self.trackball.then_some(outputs::Trackball { glide_ms: self.trackball_glide })
    }

    /// Flags that don't go together
    fn validate(&self) -> Result<()> {
        if self.trackball && !self.accel().enabled() {
            anyhow::bail!(
                "--trackball needs pointer acceleration, which --mouse-accel-min {} and --mouse-accel-knee {} turn off",
                self.mouse_accel_min,
                self.mouse_accel_knee
            );
        }
        Ok(())
    }
}

/// For --trackball-glide: a finite number above 0
fn positive(s: &str) -> Result<f32, String> {
    let v = non_negative(s)?;
    if v > 0.0 { Ok(v) } else { Err("must be above 0".into()) }
}

/// For the accel flags: a finite number, 0 or more
fn non_negative(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("{e}"))?;
    if v.is_finite() && v >= 0.0 { Ok(v) } else { Err("must be a finite number, 0 or more".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_is_on_by_default() {
        let a = Args::parse_from(["shieldd"]).accel();
        assert_eq!(a, outputs::Accel { min: 0.35, knee: 1.0 });
    }

    #[test]
    fn accel_flags_reject_nonsense() {
        for bad in [["--mouse-accel-min", "nan"], ["--mouse-accel-min", "-0.5"], ["--mouse-accel-knee", "nan"], ["--mouse-accel-knee", "-1"]] {
            assert!(Args::try_parse_from(["shieldd", bad[0], bad[1]]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn trackball_is_off_by_default() {
        let a = Args::parse_from(["shieldd"]);
        assert_eq!(a.trackball(), None);
        assert!(a.validate().is_ok());
    }

    #[test]
    fn trackball_flags_are_read() {
        let a = Args::parse_from(["shieldd", "--trackball"]);
        assert_eq!(a.trackball(), Some(outputs::Trackball { glide_ms: 350.0 }));
        assert!(a.validate().is_ok());
        let a = Args::parse_from(["shieldd", "--trackball", "--trackball-glide", "500"]);
        assert_eq!(a.trackball(), Some(outputs::Trackball { glide_ms: 500.0 }));
    }

    #[test]
    fn trackball_glide_rejects_nonsense() {
        for bad in ["0", "-1", "nan", "inf"] {
            assert!(Args::try_parse_from(["shieldd", "--trackball-glide", bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn trackball_needs_acceleration() {
        let a = Args::parse_from(["shieldd", "--trackball", "--mouse-accel-min", "1"]);
        let e = a.validate().expect_err("no acceleration, no trackball").to_string();
        assert!(e.contains("--trackball") && e.contains("--mouse-accel-min"), "{e}");
        // Without the flag, acceleration off is fine
        assert!(Args::parse_from(["shieldd", "--mouse-accel-min", "1"]).validate().is_ok());
    }

    #[test]
    fn accel_flags_are_read() {
        let a = Args::parse_from(["shieldd", "--mouse-accel-min", "0.5", "--mouse-accel-knee", "2"]).accel();
        assert_eq!(a, outputs::Accel { min: 0.5, knee: 2.0 });
    }
}

/// --audio-backend's choices
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum AudioBackend {
    Vban,
    #[cfg(target_os = "linux")]
    Pipewire,
}

impl From<AudioBackend> for shdlib::audio::Backend {
    fn from(b: AudioBackend) -> Self {
        match b {
            AudioBackend::Vban => Self::Vban,
            #[cfg(target_os = "linux")]
            AudioBackend::Pipewire => Self::PipeWire,
        }
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
    let claimed = match &parsed {
        Ok(a) if a.hosts() => Some(background::claim(!windowless)),
        _ => None,
    };
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

fn dispatch(mut args: Args, windowless: bool, claimed: Option<Quit>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if args.transmit {
            return transmit::run(args.dest, args.hidraw.as_deref(), args.dump);
        }
        if args.install_udev_rule {
            return background::install_udev_rule();
        }
    }
    #[cfg(not(target_os = "linux"))]
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
        if args.trackball {
            // Its packets carry no finger up or down
            tracing::warn!("--trackball does nothing with --receiver");
        }
        return receiver::run(args.listen, args.scale(), args.accel(), &quit);
    }
    args.validate()?;
    radio::run(args, &quit)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
