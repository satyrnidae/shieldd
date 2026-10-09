//! The tray: battery, connection and pairing at a glance, and a menu to pair, identify, rename
//! and so on. What it shows and what it sends back are platform-neutral, here and in
//! battery.rs; the tray itself is per platform, so far windows.rs.

#[cfg_attr(not(windows), allow(dead_code, reason = "only the Windows tray uses it so far"))]
mod battery;
#[cfg(windows)]
mod windows;

pub use battery::Battery;
#[cfg(windows)]
pub use windows::{Tray, device_sound};

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
    pub controller: Option<Connected>,
    pub vban: bool,
    pub pairing: bool,
    /// The group being hosted, None w/o an adapter
    pub ssid: Option<String>,
    /// Its channel, None until a survey picks one
    pub channel: Option<u8>,
    /// A controller is on its way in: associated, but not yet up w/ a battery reading
    pub joining: bool,
    /// One sent away to sleep and ignored for now. Listed while no controller's connected.
    pub shunned: Option<Shunned>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shunned {
    pub mac: [u8; 6],
    pub name: Option<String>,
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
}

/// Picked from the menu
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Pair,
    /// On or off
    Vban(bool),
    Identify,
    /// The controller w/ this MAC goes by this name now, already cleaned up
    Rename([u8; 6], String),
    Disconnect,
    Exit,
}
