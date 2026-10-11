//! The tray's menu and the text it shows: the menu for a status, each controller's line and the
//! tooltip's, and what a pick comes to, asking first where it should. Each platform draws the
//! menu and its boxes its own way.
//!
//! Menu text marks its shortcut letter as Windows does: `&` before it, and `&&` for a plain one.
//! A platform that marks them otherwise converts.

use std::sync::OnceLock;

use shdlib::audio::{self, Backend};
use shdlib::names::MAX_NAME;
use shdnet::mac::MacFmt;

use crate::battery::Battery;
use crate::tr;
use crate::{About, Command, Connected, MAX_CONTROLLERS, Signal, Status};

/// From Tray::start, for the menu's title and the About box
pub(crate) static ABOUT: OnceLock<About> = OnceLock::new();

pub(crate) fn about_info() -> About {
    ABOUT.get().copied().unwrap_or(About { version: "?", authors: "", built: "?", source: "" })
}

/// How a menu item looks, and whether it can be picked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Picked, or opens its submenu
    Normal,
    /// A line of information: can't be picked, but isn't grayed out either
    Title,
    /// Can't be picked for now
    Grayed,
    Separator,
}

/// A menu item, or a submenu with its items
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    pub(crate) kind: Kind,
    /// Ignored for a submenu
    pub(crate) id: usize,
    pub(crate) text: String,
    pub(crate) sub: Vec<Item>,
    /// The image beside the text
    pub(crate) mark: Option<Mark>,
}

/// The image beside a controller's line: its signal bars, or in their place, with its battery
/// critical, the battery warning, flashing as the tray icon does
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Mark {
    Signal(Signal),
    /// All red
    Critical,
    /// The flash's other frame: the red outline, the mark in black
    CriticalDim,
}

impl Item {
    fn new(kind: Kind, id: usize, text: impl Into<String>) -> Item {
        Item { kind, id, text: text.into(), sub: Vec::new(), mark: None }
    }

    fn separator() -> Item {
        Item::new(Kind::Separator, 0, "")
    }
}

/// Menu item IDs; 0 is nothing picked
pub(crate) mod id {
    /// The title line
    pub const ABOUT: usize = 1;
    pub const PAIR: usize = 2;
    pub const RECONFIGURE: usize = 3;
    pub const EXIT: usize = 4;
    /// A controller's submenu: CONTROLLER + its index * PER_CONTROLLER + one of the below
    pub const CONTROLLER: usize = 0x100;
    pub const PER_CONTROLLER: usize = 0x10;
    pub const IDENTIFY: usize = 0;
    pub const RENAME: usize = 1;
    pub const AUDIO: usize = 2;
    pub const DISCONNECT: usize = 3;
    pub const FORGET: usize = 4;

    pub fn of(index: usize, action: usize) -> usize {
        CONTROLLER + index * PER_CONTROLLER + action
    }

    /// A controller's index and action, from a pick
    pub fn controller(picked: usize) -> Option<(usize, usize)> {
        let n = picked.checked_sub(CONTROLLER)?;
        Some((n / PER_CONTROLLER, n % PER_CONTROLLER))
    }
}

/// `s` as menu text, where a lone & would underline the next letter. For anything we didn't
/// write: a controller's name starts out as whatever it said over WPS.
pub(crate) fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// What to call a controller: its name, or a stand-in without one
pub(crate) fn name_of(name: Option<&str>) -> String {
    name.map_or_else(|| tr!("controller-unnamed"), Into::into)
}

/// The tooltip with something connected: each controller's menu line without the port or the
/// shortcut. By port, then the order they connected in, so the newest is at the bottom; without a
/// port, after those with one. `watch` is a controller's percent shown and its connection.
/// Windows cuts the tooltip at 127 characters: four 15-character names charging at 99% come to
/// exactly that.
pub(crate) fn tip_lines(controllers: &[Connected], watch: impl Fn(&[u8; 6]) -> (Option<u8>, Option<u64>)) -> String {
    let mut lines: Vec<_> = controllers.iter().map(|c| (c, watch(&c.mac))).collect();
    lines.sort_by_key(|(c, (_, n))| (c.port.unwrap_or(u8::MAX), n.unwrap_or(u64::MAX)));
    lines
        .iter()
        .map(|(c, (pct, _))| {
            let (state, pct) = battery_args(c, *pct);
            tr!("tip-line", state = state, name = name_of(c.name.as_deref()), pct = pct)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A connected controller's line of the menu, with a shortcut letter not in `used`
fn controller_line(c: &Connected, pct: Option<u8>, used: &mut Vec<char>) -> String {
    let name = shortcut(&name_of(c.name.as_deref()), used);
    let port = c.port.map_or_else(|| "?".into(), |p| p.to_string());
    let (state, pct) = battery_args(c, pct);
    tr!("controller-line", state = state, name = name, port = port, pct = pct)
}

/// The `$state` and `$pct` a controller's battery shows with, `pct` the percent shown
fn battery_args(c: &Connected, pct: Option<u8>) -> (&'static str, u8) {
    match (c.battery, pct) {
        (Some(b), Some(p)) => (charge_state(&b, p), p),
        _ => ("unknown", 0),
    }
}

/// The `$state` a battery's level is shown with
pub(crate) fn charge_state(b: &Battery, pct: u8) -> &'static str {
    match b.charging {
        true if pct >= 100 => "charged",
        true => "charging",
        false => "level",
    }
}

/// `name` as menu text, its first letter or digit not in `used` the shortcut, which then is.
/// Without a free one, none.
fn shortcut(name: &str, used: &mut Vec<char>) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    let mut picked = false;
    for ch in name.chars() {
        let key = ch.to_lowercase().next().unwrap_or(ch);
        if !picked && ch.is_alphanumeric() && !used.contains(&key) {
            used.push(key);
            out.push('&');
            picked = true;
        }
        // A lone & would underline the next letter
        if ch == '&' {
            out.push('&');
        }
        out.push(ch);
    }
    out
}

/// The shortcut letter `text` underlines, if any
fn accel(text: &str) -> Option<char> {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.next()? {
                '&' => {}
                n => return n.to_lowercase().next(),
            }
        }
    }
    None
}

/// The menu for `s`, with each controller's percent as the tray shows it, and a critical battery's
/// warning `lit` or not
pub(crate) fn menu_items(s: &Status, pct: impl Fn(&[u8; 6]) -> Option<u8>, lit: bool) -> Vec<Item> {
    let gray = |on: bool| if on { Kind::Normal } else { Kind::Grayed };
    let title = Kind::Title;
    let mut items = vec![
        Item::new(Kind::Normal, id::ABOUT, tr!("menu-title", version = about_info().version)),
        Item::new(title, 0, format!(" > {}", tr!("menu-connected", count = s.controllers.len()))),
    ];
    if let Some(ssid) = &s.ssid {
        let ssid = menu_text(ssid);
        let line = match s.channel {
            Some(c) => format!(" > {}", tr!("menu-network", ssid = ssid, channel = c)),
            None => format!(" > {ssid}"),
        };
        items.push(Item::new(title, 0, line));
    }
    items.push(Item::separator());
    let pair = tr!("menu-pair");
    let (reconfigure, exit) = (tr!("menu-reconfigure"), tr!("menu-exit"));
    // Controllers get the letters the rest leave
    let mut used: Vec<char> = [&pair, &reconfigure, &exit].into_iter().filter_map(|t| accel(t)).collect();
    for (n, c) in s.controllers.iter().enumerate() {
        let mut sub = vec![Item::new(title, 0, tr!("controller-mac", mac = c.mac.fmt()))];
        if let Some(v) = &c.firmware {
            sub.push(Item::new(title, 0, tr!("controller-firmware", version = menu_text(v))));
        }
        sub.extend([
            Item::separator(),
            Item::new(Kind::Normal, id::of(n, id::IDENTIFY), tr!("controller-identify")),
            Item::new(Kind::Normal, id::of(n, id::RENAME), tr!("controller-rename")),
            Item::new(
                Kind::Normal,
                id::of(n, id::AUDIO),
                if c.audio { tr!("controller-audio-off") } else { tr!("controller-audio-on") },
            ),
            Item::new(Kind::Normal, id::of(n, id::DISCONNECT), tr!("controller-disconnect")),
            Item::separator(),
            Item::new(Kind::Normal, id::of(n, id::FORGET), tr!("controller-forget")),
        ]);
        let line =
            Item { sub, mark: mark(c, lit), ..Item::new(Kind::Normal, 0, controller_line(c, pct(&c.mac), &mut used)) };
        items.push(line);
    }
    if s.controllers.is_empty() {
        items.push(Item::new(gray(false), 0, tr!("menu-none-connected")));
    }
    items.extend([
        Item::separator(),
        // Already open, or no room for another
        Item::new(gray(!s.pairing && s.controllers.len() < MAX_CONTROLLERS), id::PAIR, pair),
        Item::new(Kind::Normal, id::RECONFIGURE, reconfigure),
        Item::separator(),
        Item::new(Kind::Normal, id::EXIT, exit),
    ]);
    items
}

/// The image beside `c`'s line, the warning `lit` or not
fn mark(c: &Connected, lit: bool) -> Option<Mark> {
    match c.battery {
        Some(b) if b.critical && !b.charging => Some(if lit { Mark::Critical } else { Mark::CriticalDim }),
        _ => c.signal.map(Mark::Signal),
    }
}

/// The boxes a pick from the menu may open, as each platform shows them
pub(crate) trait Dialogs {
    /// Shutting down: no more boxes
    fn quitting(&self) -> bool;
    /// Something to know, with just OK
    fn inform(&self, title: &str, text: &str);
    /// As inform, with `url`, where it's in `text`, a link that opens it
    fn inform_link(&self, title: &str, text: &str, url: &str);
    /// OK or Cancel. `warn` for something that can't be undone, which leans to Cancel.
    fn confirm(&self, title: &str, text: &str, warn: bool) -> bool;
    /// A controller's new name, up to MAX_NAME characters, starting from `name`. None if
    /// cancelled.
    fn ask_name(&self, title: &str, prompt: &str, name: &str) -> Option<String>;
}

/// What picking `picked` from the menu for `s` comes to, asking first where it should. None for
/// nothing: nothing picked, cancelled, or the About box.
pub(crate) fn command(picked: usize, s: &Status, d: &impl Dialogs) -> Option<Command> {
    let cmd = match (picked, id::controller(picked)) {
        _ if d.quitting() => return None,
        (id::PAIR, _) if confirm_pair(d) => Command::Pair,
        (id::RECONFIGURE, _) if confirm_reconfigure(d) => Command::Reconfigure,
        (id::EXIT, _) => Command::Exit,
        (id::ABOUT, _) => {
            about(d);
            return None;
        }
        (_, Some((n, action))) => {
            let c = s.controllers.get(n)?;
            match action {
                id::IDENTIFY => Command::Identify(c.mac),
                id::RENAME => Command::Rename(c.mac, rename(d, c)?),
                id::AUDIO => Command::Audio(c.mac, !c.audio),
                id::DISCONNECT => Command::Disconnect(c.mac),
                id::FORGET if confirm_forget(d, c) => Command::Forget(c.mac),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(cmd)
}

/// Once `cmd`, picked from the menu for `s`, has gone out: the first audio on starts the shared
/// mic and headset, so say where they are
pub(crate) fn follow_up(cmd: &Command, s: &Status, d: &impl Dialogs) {
    let first_on = matches!(cmd, Command::Audio(_, true)) && !s.controllers.iter().any(|c| c.audio);
    if !first_on || d.quitting() {
        return;
    }
    match s.audio {
        Backend::Vban => {
            use shdlib::audio::vban;
            let text = tr!(
                "vban-on",
                mic_stream = vban::MIC_STREAM,
                mic_port = vban::MIC_PORT,
                pad_stream = vban::PAD_STREAM,
                pad_port = vban::PAD_PORT,
            );
            d.inform(&tr!("vban-on-title"), &text);
        }
        Backend::PipeWire => {
            let text = tr!("pipewire-on", mic = audio::MIC_NAME, headset = audio::HEADSET_NAME);
            d.inform(&tr!("pipewire-on-title"), &text);
        }
    }
}

/// Asks for a new name for `c`. Returns it cleaned up, or None if cancelled or left blank.
fn rename(d: &impl Dialogs, c: &Connected) -> Option<String> {
    let prompt = tr!("rename-prompt", mac = c.mac.fmt(), max = MAX_NAME);
    let name = d.ask_name(&tr!("rename-title"), &prompt, c.name.as_deref().unwrap_or(""))?;
    shdlib::names::clean(&name)
}

/// Version, build, author, and the notices the GPL and the bundled works' licenses ask for
fn about(d: &impl Dialogs) {
    let info = about_info();
    let author = info.authors.replace(':', ", ");
    let text = tr!("about", version = info.version, built = info.built, author = author, source = info.source);
    d.inform_link(&tr!("about-title"), &text, info.source);
}

/// How to pair, before turning it on
fn confirm_pair(d: &impl Dialogs) -> bool {
    d.confirm(&tr!("pair-title"), &tr!("pair"), false)
}

/// The controller forgets the network, so ask first
fn confirm_forget(d: &impl Dialogs, c: &Connected) -> bool {
    d.confirm(&tr!("forget-title"), &tr!("forget", name = name_of(c.name.as_deref())), true)
}

/// A new network throws every pairing away, so ask first
fn confirm_reconfigure(d: &impl Dialogs) -> bool {
    d.confirm(&tr!("reconfigure-title"), &tr!("reconfigure"), true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn up(mac: u8, name: &str, audio: bool) -> Connected {
        let battery = Battery { charging: false, band: 50, reading: 0xD500, critical: false, must_charge: false };
        Connected {
            mac: [mac; 6],
            name: Some(name.into()),
            port: Some(mac),
            battery: Some(battery),
            audio,
            signal: Some(Signal::Good),
            firmware: Some("1.96/99/3.71/0.32".into()),
        }
    }

    #[test]
    fn lines() {
        let c = Connected { name: Some("Pad&Co".into()), port: Some(4), ..Default::default() };
        assert_eq!(controller_line(&c, None, &mut vec![]), "&Pad&&Co: Port #4");
        assert_eq!(controller_line(&up(2, "A", false), Some(40), &mut vec![]), "&A: Port #2 (40%)");
        // Past what's taken, and past & and other non-letters
        let mut used = vec!['p', 'c', 'x'];
        assert_eq!(shortcut("Pad", &mut used), "P&ad");
        assert_eq!(shortcut("Pal", &mut used), "Pa&l");
        assert_eq!(shortcut("Controller", &mut used), "C&ontroller");
        assert_eq!(shortcut("P&A-1", &mut used), "P&&A-&1");
        assert_eq!(shortcut("pal", &mut used), "pal", "nothing left");
        assert_eq!(used, ['p', 'c', 'x', 'a', 'l', 'o', '1']);
        assert_eq!(accel("&Pair controller..."), Some('p'));
        assert_eq!(accel("Pad&&Co"), None);
    }

    #[test]
    fn a_submenu_per_controller() {
        let s = Status {
            // A's setup hasn't read its firmware yet
            controllers: vec![Connected { firmware: None, ..up(1, "A", false) }, up(2, "B", true)],
            ..Default::default()
        };
        let items = menu_items(&s, |m| Some(m[0] * 10), true);
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        // The bars go on each controller's line and nowhere else
        let marks: Vec<_> = items.iter().filter_map(|i| i.mark).collect();
        assert_eq!(marks, [Mark::Signal(Signal::Good), Mark::Signal(Signal::Good)]);
        assert_eq!(items[3].mark, Some(Mark::Signal(Signal::Good)));
        assert_eq!(
            texts,
            [
                &format!("shieldd (v{})", about_info().version),
                " > 2 connected.",
                "",
                "&A: Port #1 (10%)",
                "&B: Port #2 (20%)",
                "",
                "&Pair controller...",
                "Re&configure network",
                "",
                "E&xit",
            ]
        );
        let b = &items[4];
        let sub: Vec<_> = b.sub.iter().map(|i| (i.id, i.text.as_str())).collect();
        assert_eq!(
            sub,
            [
                (0, "MAC 02:02:02:02:02:02"),
                (0, "Firmware v1.96/99/3.71/0.32"),
                (0, ""),
                (id::of(1, id::IDENTIFY), "&Identify"),
                (id::of(1, id::RENAME), "&Rename..."),
                (id::of(1, id::AUDIO), "Disable &audio"),
                (id::of(1, id::DISCONNECT), "&Disconnect"),
                (0, ""),
                (id::of(1, id::FORGET), "&Forget"),
            ]
        );
        assert!(!items[3].sub.iter().any(|i| i.text.starts_with("Firmware")));
        assert_eq!(id::controller(id::of(1, id::AUDIO)), Some((1, id::AUDIO)));
        assert_eq!(id::controller(id::EXIT), None);
        // Pairing grays its line; with nothing at all there's a line to say so
        let none = menu_items(&Status { pairing: true, ..Default::default() }, |_| None, true);
        let at = |t: &str| none.iter().find(|i| i.text == t).unwrap().kind == Kind::Grayed;
        assert!(at("No controllers connected") && at("&Pair controller..."));
        // Four connected: no room to pair a fifth
        let pair_grayed = |n: u8| {
            let s = Status { controllers: (1..=n).map(|m| up(m, "A", false)).collect(), ..Default::default() };
            let items = menu_items(&s, |_| Some(50), true);
            items.iter().find(|i| i.id == id::PAIR).unwrap().kind == Kind::Grayed
        };
        assert!(!pair_grayed(3));
        assert!(pair_grayed(4));
    }

    #[test]
    fn a_critical_battery_flashes_in_place_of_the_bars() {
        let critical = |charging: bool| {
            let mut c = up(1, "A", false);
            c.battery = c.battery.map(|b| Battery { band: 10, critical: true, charging, ..b });
            c
        };
        let s = Status { controllers: vec![critical(false), up(2, "B", false)], ..Default::default() };
        let marks = |lit: bool| -> Vec<_> { menu_items(&s, |_| Some(5), lit).iter().filter_map(|i| i.mark).collect() };
        assert_eq!(marks(true), [Mark::Critical, Mark::Signal(Signal::Good)]);
        assert_eq!(marks(false), [Mark::CriticalDim, Mark::Signal(Signal::Good)]);
        // On the charger it's no longer running out: the bars are back
        let s = Status { controllers: vec![critical(true)], ..Default::default() };
        assert_eq!(menu_items(&s, |_| Some(5), true)[3].mark, Some(Mark::Signal(Signal::Good)));
        // Still flagged with no signal reading to replace
        let s = Status { controllers: vec![Connected { signal: None, ..critical(false) }], ..Default::default() };
        assert_eq!(menu_items(&s, |_| Some(5), true)[3].mark, Some(Mark::Critical));
    }

    #[test]
    fn tip_by_port_then_connection() {
        let mut cs = [up(1, "A", false), up(2, "B", false), up(3, "C", false), up(4, "D", false)];
        cs[0].port = Some(2);
        cs[1].port = None;
        cs[2].port = Some(1);
        cs[3].port = None;
        cs[1].battery = None;
        let watch = |m: &[u8; 6]| (Some(10 * m[0]), Some([0, 5, 9, 7, 2][usize::from(m[0])]));
        assert_eq!(
            tip_lines(&cs, watch),
            "C (30%)\nA (10%)\nD (40%)\nB",
            "no port goes last, the newest at the bottom"
        );
    }
}
