//! The tray: battery, connection and pairing at a glance, and a menu to pair, reconfigure the
//! network, and identify, rename, stream or disconnect each controller. What it shows and what
//! it sends back are platform-neutral: here, battery.rs, icons.rs, menu.rs and model.rs, with
//! handle.rs the side shieldd holds. Putting it on screen is per platform: windows.rs, and
//! linux.rs with the `tray` feature, which build.rs marks `gui`. Headless, for --no-tray or a
//! build without one, headless.rs only logs.

mod battery;
mod handle;
mod headless;
#[cfg_attr(not(gui), allow(dead_code, reason = "headless"))]
mod icons;
pub mod l10n;
#[cfg(all(gui, target_os = "linux"))]
mod linux;
#[cfg_attr(not(gui), allow(dead_code, reason = "headless"))]
mod menu;
#[cfg_attr(not(gui), allow(dead_code, reason = "headless"))]
mod model;
#[cfg(windows)]
mod windows;

pub use battery::Battery;
pub use handle::Tray;
#[cfg(all(gui, target_os = "linux"))]
use linux as platform;
#[cfg(windows)]
use windows as platform;

/// XInput's four ports: as many controllers as can be hosted at once
pub const MAX_CONTROLLERS: usize = 4;

/// What the About box tells, from the program it's in
#[derive(Debug, Clone, Copy)]
pub struct About {
    pub version: &'static str,
    /// As Cargo has them, `:` between authors
    pub authors: &'static str,
    pub built: &'static str,
}

/// What the tray shows
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// Up to four, in the order they connected
    pub controllers: Vec<Connected>,
    pub pairing: bool,
    /// The group being hosted, None without an adapter
    pub ssid: Option<String>,
    /// Its channel, None until a survey picks one
    pub channel: Option<u8>,
    /// A controller is on its way in: associated, but not yet up with a battery reading
    pub joining: bool,
    /// A controller shieldd sent away on purpose, by Disconnect, Forget or for its battery,
    /// hasn't come back yet
    pub sent_away: bool,
}

/// How well a controller's link is holding up, as the bars by its line in the menu
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// About to drop, or nothing heard from it lately: no bars
    Dropping,
    /// One red bar
    Low,
    /// OK: two yellow bars
    Fair,
    /// All three, green
    Good,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Connected {
    pub mac: [u8; 6],
    /// From known-clients: its model at pairing, or what it was renamed to. Unknown for one
    /// paired before shieldd kept track.
    pub name: Option<String>,
    /// XInput port, 1-4, once Windows has assigned one
    pub port: Option<u8>,
    /// Once the first status answer is in
    pub battery: Option<Battery>,
    /// Its mic and headset are on the VBAN streams
    pub vban: bool,
    /// Its link's signal bars; shieldd's radio host always says. None shows none.
    pub signal: Option<Signal>,
    /// Firmware version, `1.96/99/3.71/0.32`, once setup has read it
    pub firmware: Option<String>,
}

/// Picked from the menu. The ones for a controller carry its MAC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// One more controller onto the network as it is
    Pair,
    /// A new network, which every controller pairs to again
    Reconfigure,
    Identify([u8; 6]),
    /// Goes by this name now, already cleaned up
    Rename([u8; 6], String),
    /// VBAN on or off for it
    Vban([u8; 6], bool),
    Disconnect([u8; 6]),
    /// Clears its pairing; it has to pair again
    Forget([u8; 6]),
    Exit,
}
