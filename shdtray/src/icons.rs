//! The tray icon's pictures: battery, disconnected, pairing and spinner icons with their
//! animation frames and count badges, and the menu's signal bars. SVGs from res/, recolored for
//! a light or dark panel and rendered to pixmaps; each platform turns those into its own icons.
//!
//! Battery icons are Ikono Bold Line Icons, turned upright; the disconnected and pairing ones
//! are from Clarity. All from shdtray/res/, MIT, see THIRD_PARTY.md.

use std::time::Duration;

use anyhow::{Context, Result};
use resvg::tiny_skia::{Pixmap, PixmapPaint, Transform};
use resvg::usvg;

use crate::battery::Battery;
use crate::menu::Mark;
use crate::{MAX_CONTROLLERS, Signal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Icon {
    /// On the charger: outline and bolt in the stroke color. Flashes with ChargingLit when plugged in.
    Charging,
    /// The plug-in flash's other frame: the bolt in green
    ChargingLit,
    /// On the charger at 100%: the full battery, outline and all in green
    Charged,
    Full,
    Medium,
    Low,
    Empty,
    /// Critical: all red
    Critical,
    /// Critical's other frame: the red outline with the exclamation mark in the stroke color, so it
    /// flashes
    CriticalDim,
    Disconnected,
    /// Disconnected's other frame, all in red, so it flashes
    DisconnectedRed,
    /// Frame 0 to PAIRING_FRAMES - 1 of the pairing animation
    Pairing(u8),
    /// Frame 0 to SPINNER_FRAMES - 1 of the spinner, while a controller is on its way in
    Spinner(u8),
}

/// Spinner speed: a turn every 1.6 s
const SPINNER_DEG_PER_S: f32 = 225.0;
/// Each frame is a call into the panel (Explorer, on Windows), so no more than it takes to look
/// smooth
const SPINNER_FPS: u64 = 15;
/// How far it turns each frame
const SPINNER_STEP: f32 = SPINNER_DEG_PER_S / SPINNER_FPS as f32;
const SPINNER_FRAMES: u8 = (360.0 / SPINNER_STEP) as u8;
const SPINNER_FRAME: Duration = Duration::from_nanos(1_000_000_000 / SPINNER_FPS);
/// The spinner's arc, in its 24-unit viewBox: centered, radius 8 plus half its 3.56 stroke.
/// It's fit by this circle rather than by the arc, which would wobble as it turns.
const SPINNER_RADIUS: f32 = 8.0 + 3.55556 / 2.0;

/// The pairing animation: a blank frame, then the Wi-Fi icon's dot and its arcs inside out
/// light up blue one at a time
const PAIRING_FRAMES: u8 = 5;
/// How long each pairing frame shows
const PAIRING_FRAME: Duration = Duration::from_millis(400);
/// How long each half of a flash shows: the charging bolt, the critical mark, the disconnected
/// icon
pub(crate) const FLASH_FRAME: Duration = Duration::from_millis(800);
/// How long a flash runs before the icon holds still: the disconnected one at startup and each
/// time a controller leaves on its own, rather than sent away; the charging bolt each time
/// charging starts
pub(crate) const FLASH_FOR: Duration = Duration::from_secs(5);
/// How many controllers the corner badge can count: one has none, and four is all there can be
pub(crate) const BADGES: std::ops::RangeInclusive<u8> = 2..=MAX_CONTROLLERS as u8;
/// The badge's side, as a share of the icon's
const BADGE_SCALE: f32 = 5.0 / 8.0;

/// The sides the icon is rendered at, the panel picking the one that suits it, and how much of
/// each the drawing takes. Theme icons keep to about three quarters of theirs, Breeze's 16 of 22,
/// so ours does too; its count badge stays full size, to read.
#[cfg(target_os = "linux")]
pub(crate) const SIZES: [(u32, u32); 2] = [(24, 18), (48, 36)];
/// Where the ring around each Phosphor digit ends, the pen back at (128, 216). The number's
/// first move is relative to that.
const BADGE_RING_END: &str = "Zm0,192a88,88,0,1,1,88-88A88.09957,88.09957,0,0,1,128,216Z";

/// The wifi icon's elements, by class, in the order they light up: the dot, then the arcs
/// inside out
const PAIRING_ORDER: [&str; 4] = ["path-1", "path-4", "path-3", "path-2"];

#[derive(Debug, Clone, Copy)]
enum Accent {
    None,
    Green,
    Yellow,
    Red,
}

impl Icon {
    const FIXED: [Icon; 16] = [
        Icon::Charging,
        Icon::ChargingLit,
        Icon::Charged,
        Icon::Full,
        Icon::Medium,
        Icon::Low,
        Icon::Empty,
        Icon::Critical,
        Icon::CriticalDim,
        Icon::Disconnected,
        Icon::DisconnectedRed,
        Icon::Pairing(0),
        Icon::Pairing(1),
        Icon::Pairing(2),
        Icon::Pairing(3),
        Icon::Pairing(4),
    ];

    /// Every icon and every animation frame
    pub(crate) fn all() -> impl Iterator<Item = Icon> {
        Icon::FIXED.into_iter().chain((0..SPINNER_FRAMES).map(Icon::Spinner))
    }

    /// For `b`, with `pct` the percent shown
    pub(crate) fn of(b: &Battery, pct: u8) -> Icon {
        if b.charging && pct >= 100 {
            Icon::Charged
        } else if b.charging {
            Icon::Charging
        } else if b.critical {
            Icon::Critical
        } else {
            // The controller's own bands: 100, 75, 50, 25, 10 and 0 seen so far
            match b.band {
                75.. => Icon::Full,
                50..=74 => Icon::Medium,
                25..=49 => Icon::Low,
                _ => Icon::Empty,
            }
        }
    }

    /// The SVG, and the color of every path after the first, which is the outline
    fn source(self) -> (&'static str, Accent) {
        match self {
            Icon::Charging => (include_str!("../res/battery-charging.svg"), Accent::None),
            Icon::ChargingLit => (include_str!("../res/battery-charging.svg"), Accent::Green),
            Icon::Charged => (include_str!("../res/battery-full.svg"), Accent::Green),
            Icon::Full => (include_str!("../res/battery-full.svg"), Accent::Green),
            Icon::Medium => (include_str!("../res/battery-medium.svg"), Accent::Yellow),
            Icon::Low => (include_str!("../res/battery-low.svg"), Accent::Red),
            Icon::Empty => (include_str!("../res/battery-empty.svg"), Accent::Red),
            Icon::Critical => (include_str!("../res/battery-critical.svg"), Accent::Red),
            // Its color is on the root element, before any path; all of it is outline
            Icon::Disconnected => (include_str!("../res/disconnected.svg"), Accent::None),
            Icon::DisconnectedRed => (include_str!("../res/disconnected.svg"), Accent::Red),
            Icon::CriticalDim => (include_str!("../res/battery-critical.svg"), Accent::None),
            // Colored by pairing_frame instead
            Icon::Pairing(_) => (include_str!("../res/pairing.svg"), Accent::None),
            // Turned by pixmap
            Icon::Spinner(_) => (include_str!("../res/spinner.svg"), Accent::None),
        }
    }

    /// The animation frame after this one, for the animated icons
    pub(crate) fn next_frame(self) -> Option<Icon> {
        match self {
            Icon::Charging => Some(Icon::ChargingLit),
            Icon::ChargingLit => Some(Icon::Charging),
            Icon::Critical => Some(Icon::CriticalDim),
            Icon::CriticalDim => Some(Icon::Critical),
            Icon::Disconnected => Some(Icon::DisconnectedRed),
            Icon::DisconnectedRed => Some(Icon::Disconnected),
            Icon::Pairing(n) => Some(Icon::Pairing((n + 1) % PAIRING_FRAMES)),
            Icon::Spinner(n) => Some(Icon::Spinner((n + 1) % SPINNER_FRAMES)),
            _ => None,
        }
    }

    /// How long this frame of an animation shows
    pub(crate) fn frame_time(self) -> Duration {
        match self {
            Icon::Pairing(_) => PAIRING_FRAME,
            Icon::Spinner(_) => SPINNER_FRAME,
            _ => FLASH_FRAME,
        }
    }

    /// `want`, or the frame of it already on screen, so an update doesn't restart the animation
    pub(crate) fn keep_frame(want: Icon, shown: Option<Icon>) -> Icon {
        match (want, shown) {
            (Icon::Charging, Some(Icon::ChargingLit)) => Icon::ChargingLit,
            (Icon::Critical, Some(Icon::CriticalDim)) => Icon::CriticalDim,
            (Icon::Disconnected, Some(Icon::DisconnectedRed)) => Icon::DisconnectedRed,
            (Icon::Pairing(_), Some(i @ Icon::Pairing(_))) => i,
            (Icon::Spinner(_), Some(i @ Icon::Spinner(_))) => i,
            _ => want,
        }
    }

    /// The SVG in the taskbar's colors
    fn svg(self, p: &Palette) -> String {
        let (svg, accent) = self.source();
        match self {
            Icon::Pairing(n) => pairing_frame(svg, p, n),
            // All of it in the accent, outline included, which colorize keeps for the stroke
            Icon::DisconnectedRed | Icon::Charged | Icon::Critical | Icon::Empty => {
                svg.replace("#000000", p.accent(accent))
            }
            // Critical's outline with the mark dimmed to the stroke color
            Icon::CriticalDim => paint(svg, p.red, p.stroke),
            _ => colorize(svg, p, accent),
        }
    }

    /// Rendered at `size` in the taskbar's colors
    fn pixmap(self, p: &Palette, size: u32) -> Result<Pixmap> {
        match self {
            Icon::Spinner(n) => rasterize_turned(&self.svg(p), size, f32::from(n) * SPINNER_STEP),
            _ => rasterize(&self.svg(p), size),
        }
    }

    /// One of the battery icons, as opposed to the signal, pairing and spinner ones
    fn is_battery(self) -> bool {
        !matches!(self, Icon::Disconnected | Icon::DisconnectedRed | Icon::Pairing(_) | Icon::Spinner(_))
    }

    /// Whether it can carry a count badge: not with nothing connected
    pub(crate) fn takes_badge(self) -> bool {
        !matches!(self, Icon::Disconnected | Icon::DisconnectedRed)
    }

    /// As pixmap on a `size` square, with the count badge in the lower right corner, if any. The
    /// drawing itself is `art` across: all of it at `size`, or less, centered, to leave a margin.
    /// With a badge, a smaller drawing moves out from under it, a battery to the left and the rest
    /// to the top left; the badge stays its share of the whole square.
    pub(crate) fn badged(self, p: &Palette, size: u32, art: u32, badge: Option<u8>) -> Result<Pixmap> {
        // A battery's narrow enough to step aside to the left, out from under the badge
        let drawn = match badge.is_some() && self.is_battery() {
            true => fit(&self.svg(p), art, false),
            false => self.pixmap(p, art),
        }?;
        let mut pm = match art < size {
            true => {
                let mut pm = Pixmap::new(size, size).context("icon pixmap")?;
                let mid = ((size - art) / 2) as i32;
                let (x, y) = match badge {
                    None => (mid, mid),
                    Some(_) if self.is_battery() => (0, mid),
                    Some(_) => (0, 0),
                };
                pm.draw_pixmap(x, y, drawn.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                pm
            }
            false => drawn,
        };
        let Some(n) = badge else { return Ok(pm) };
        let svg = badge_svg(n, p).with_context(|| format!("no badge for {n}"))?;
        let side = ((size as f32 * BADGE_SCALE).round() as u32).max(1);
        let b = rasterize(&svg, side)?;
        let at = (size - side) as i32;
        pm.draw_pixmap(at, at, b.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        Ok(pm)
    }
}

/// The count badge for `n` controllers, from Phosphor's duotone digits: the disc in blue at
/// full strength, the ring around it dropped, and the number in the stroke color
fn badge_svg(n: u8, p: &Palette) -> Option<String> {
    let svg = match n {
        2 => include_str!("../res/badge-two.svg"),
        3 => include_str!("../res/badge-three.svg"),
        4 => include_str!("../res/badge-four.svg"),
        _ => return None,
    };
    let (head, number) = svg.split_once(BADGE_RING_END)?;
    let d = head.rfind("d=\"")? + 3;
    let svg = format!("{}M128,216{number}", &head[..d]);
    let disc = format!("<circle fill=\"{}\" ", p.blue);
    Some(svg.replace("<g opacity=\"0.2\">", "<g>").replacen("<circle ", &disc, 1).replace("#000000", p.stroke))
}

/// The menu's critical battery warning, in place of the signal bars
const WARNING_SVG: &str = include_str!("../res/battery-critical-menu.svg");

/// The image for `m` beside a controller's line in the menu, which stays light whatever the
/// taskbar does
pub(crate) fn mark_svg(m: Mark) -> String {
    match m {
        Mark::Signal(s) => signal_svg(s),
        Mark::Critical => paint(WARNING_SVG, LIGHT.red, LIGHT.red),
        Mark::CriticalDim => paint(WARNING_SVG, LIGHT.red, LIGHT.stroke),
    }
}

/// The menu's signal bars, Ikono's graph-asc
const SIGNAL_SVG: &str = include_str!("../res/signal-strength.svg");
/// The bars that aren't lit, pale enough on the light menu to read as off
const SIGNAL_UNLIT: &str = "#C8C8C8";

/// The signal bars for `s`, lit from the left: none, one red, two yellow or all three green.
/// The menu isn't themed and stays light whatever the taskbar does, so the light palette.
fn signal_svg(s: Signal) -> String {
    let (lit, color) = match s {
        Signal::Dropping => (0, SIGNAL_UNLIT),
        Signal::Low => (1, LIGHT.red),
        Signal::Fair => (2, LIGHT.yellow),
        Signal::Good => (3, LIGHT.green),
    };
    let (head, bars) = signal_bars();
    let mut out = head.to_owned();
    for (place, p) in bars {
        out.push_str("<path");
        out.push_str(&p.replace("#000000", if place < lit { color } else { SIGNAL_UNLIT }));
    }
    out
}

/// The signal SVG split at each path: what's before the first, then each path, after its
/// "<path", with its bar's place across, 0 the leftmost. The paths aren't in order across; a
/// bar's place is where it starts, d="M<x> ...".
fn signal_bars() -> (&'static str, Vec<(usize, &'static str)>) {
    let mut parts = SIGNAL_SVG.split("<path");
    let head = parts.next().unwrap_or_default();
    let paths: Vec<&str> = parts.collect();
    let x = |p: &str| -> f32 {
        let d = p.split("d=\"M").nth(1).unwrap_or_default();
        d.split([' ', 'L', 'V']).next().and_then(|n| n.parse().ok()).unwrap_or(0.0)
    };
    let mut across: Vec<(f32, usize)> = paths.iter().enumerate().map(|(i, p)| (x(p), i)).collect();
    across.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut places = vec![0; paths.len()];
    for (place, &(_, i)) in across.iter().enumerate() {
        places[i] = place;
    }
    (head, places.into_iter().zip(paths).collect())
}

/// Taskbar colors for one theme
pub(crate) struct Palette {
    stroke: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    /// The pairing animation
    blue: &'static str,
}

/// Only the stroke and yellow differ by theme; the darker reds and greens were too close to black
/// to flash against it
pub(crate) const LIGHT: Palette =
    Palette { stroke: "#000000", green: "#1bad05", yellow: "#daa000", red: "#db0505", blue: "#1f86e7" };
pub(crate) const DARK: Palette =
    Palette { stroke: "#FFFFFF", green: "#1ccb00", yellow: "#fec41e", red: "#fe1e1e", blue: "#40a4ff" };

impl Palette {
    fn accent(&self, a: Accent) -> &'static str {
        match a {
            Accent::None => self.stroke,
            Accent::Green => self.green,
            Accent::Yellow => self.yellow,
            Accent::Red => self.red,
        }
    }
}

/// The icons are drawn in #000000 throughout. Path 1 (Inkscape's numbering, document order)
/// and anything before it is the outline and takes the stroke color; the rest are the level
/// and take the accent.
fn colorize(svg: &str, p: &Palette, accent: Accent) -> String {
    paint(svg, p.stroke, p.accent(accent))
}

/// The outline, path 1 and anything before it, in `outline`; the rest in `fill`
fn paint(svg: &str, outline: &str, fill: &str) -> String {
    let mut parts = svg.split("<path");
    let mut out = parts.next().unwrap_or_default().replace("#000000", outline);
    for (i, part) in parts.enumerate() {
        out.push_str("<path");
        out.push_str(&part.replace("#000000", if i == 0 { outline } else { fill }));
    }
    out
}

/// The wifi icon in the stroke color, which it gets from the root element, with element `n` of
/// PAIRING_ORDER, counting from 1, in blue. Frame 0 has none.
fn pairing_frame(svg: &str, p: &Palette, n: u8) -> String {
    let out = svg.replace("#000000", p.stroke);
    let Some(class) = usize::from(n).checked_sub(1).and_then(|i| PAIRING_ORDER.get(i)) else { return out };
    let at = format!("class=\"clr-i-solid clr-i-solid-{class}\"");
    out.replace(&at, &format!("fill=\"{}\" {at}", p.blue))
}

/// The drawing itself, stroke included, fit to a `size` square and centered; the canvas has
/// margins
pub(crate) fn rasterize(svg: &str, size: u32) -> Result<Pixmap> {
    fit(svg, size, true)
}

/// As rasterize, but against the left edge unless `center`
fn fit(svg: &str, size: u32, center: bool) -> Result<Pixmap> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).context("parsing icon")?;
    let mut pm = Pixmap::new(size, size).context("icon pixmap")?;
    let bb = tree.root().abs_stroke_bounding_box();
    let side = size as f32;
    let scale = side / bb.width().max(bb.height());
    let tx = if center { (side - bb.width() * scale) / 2.0 } else { 0.0 } - bb.x() * scale;
    let ty = (side - bb.height() * scale) / 2.0 - bb.y() * scale;
    resvg::render(&tree, Transform::from_row(scale, 0.0, 0.0, scale, tx, ty), &mut pm.as_mut());
    Ok(pm)
}

/// The spinner turned `deg` clockwise about its center, fit by SPINNER_RADIUS
fn rasterize_turned(svg: &str, size: u32, deg: f32) -> Result<Pixmap> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).context("parsing icon")?;
    let mut pm = Pixmap::new(size, size).context("icon pixmap")?;
    // The tree is in canvas units; the viewBox is 24 across
    let c = tree.size().width() / 2.0;
    let r = SPINNER_RADIUS / 12.0 * c;
    let side = size as f32;
    let scale = side / (2.0 * r);
    let t = Transform::from_rotate_at(deg, c, c)
        .post_translate(-c, -c)
        .post_scale(scale, scale)
        .post_translate(side / 2.0, side / 2.0);
    resvg::render(&tree, t, &mut pm.as_mut());
    Ok(pm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_frames_light_one_at_a_time() {
        let lit = |n| {
            let svg = Icon::Pairing(n).svg(&DARK);
            PAIRING_ORDER.iter().position(|c| svg.contains(&format!("\" class=\"clr-i-solid clr-i-solid-{c}\"")))
        };
        assert_eq!((0..PAIRING_FRAMES).map(lit).collect::<Vec<_>>(), [None, Some(0), Some(1), Some(2), Some(3)]);
        let blues = |n| Icon::Pairing(n).svg(&DARK).matches(DARK.blue).count();
        assert!((1..PAIRING_FRAMES).all(|n| blues(n) == 1));
        let one = Icon::Pairing(1).svg(&DARK);
        assert!(one.contains(&format!("fill=\"{}\" class=\"clr-i-solid clr-i-solid-path-1\"", DARK.blue)));
        assert!(!one.contains("#000000"));
    }

    #[test]
    fn colors_by_path_order() {
        let (svg, accent) = Icon::Full.source();
        let s = colorize(svg, &DARK, accent);
        assert!(!s.contains("#000000"));
        assert_eq!(s.matches(DARK.stroke).count(), 1);
        assert_eq!(s.matches(DARK.green).count(), 3);
        let (svg, accent) = Icon::Empty.source();
        assert_eq!(colorize(svg, &LIGHT, accent).matches(LIGHT.stroke).count(), 1);
    }

    #[test]
    fn every_icon_renders() {
        for p in [&LIGHT, &DARK] {
            for icon in Icon::all() {
                let pm = icon.pixmap(p, 32).unwrap();
                assert!(pm.pixels().iter().any(|px| px.alpha() > 0), "{icon:?} is blank");
            }
        }
    }

    /// target/tests/icons, next to the profile folder this test binary runs from
    fn icon_dir() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        // target/<profile>/deps/shdtray-<hash>.exe
        exe.ancestors().nth(3).unwrap().join("tests").join("icons")
    }

    /// What renders_icons_to_target draws: side and drawing. Windows' is a big one to look at;
    /// Linux's are each of the sizes the panel gets, margins and all.
    #[cfg(target_os = "windows")]
    const RENDERS: [(u32, u32); 1] = [(64, 64)];
    #[cfg(target_os = "linux")]
    const RENDERS: [(u32, u32); 2] = SIZES;

    /// Every icon in both themes, on its taskbar color, for eyeballing: one PNG each, and strips
    /// of the status icons, the pairing animation and the spinner. Each strip's first row is
    /// plain, then a row for each count badge; the icons that take none leave those rows empty.
    /// Once for each of RENDERS.
    #[test]
    fn renders_icons_to_target() {
        for (size, art) in RENDERS {
            // More than one, each in a folder named for its size
            let dir = match RENDERS.len() {
                1 => icon_dir(),
                _ => icon_dir().join(size.to_string()),
            };
            render_icons(&dir, size, art);
        }
    }

    fn render_icons(dir: &std::path::Path, size: u32, art: u32) {
        const GAP: u32 = 8;
        let badges: Vec<Option<u8>> = std::iter::once(None).chain(BADGES.map(Some)).collect();
        let rows = badges.len() as u32;
        for (theme, p, bg) in [("light", &LIGHT, [0xEE, 0xEE, 0xEE]), ("dark", &DARK, [0x20, 0x20, 0x20])] {
            let out = dir.join(theme);
            std::fs::create_dir_all(&out).unwrap();
            let bg = resvg::tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], 0xFF);
            let strip_of = |n: u32| {
                let mut s = Pixmap::new(n * size + (n - 1) * GAP, rows * size + (rows - 1) * GAP).unwrap();
                s.fill(resvg::tiny_skia::Color::from_rgba8(0x80, 0x80, 0x80, 0xFF));
                s
            };
            let on_bg = |icon: Icon, badge: Option<u8>| {
                let mut pm = Pixmap::new(size, size).unwrap();
                pm.fill(bg);
                let fg = icon.badged(p, size, art, badge).unwrap();
                pm.draw_pixmap(0, 0, fg.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                pm
            };
            let statuses = Icon::FIXED.iter().filter(|i| !matches!(i, Icon::Pairing(_))).count() as u32;
            let mut status_strip = strip_of(statuses);
            let mut strip = strip_of(u32::from(PAIRING_FRAMES));
            // Every eighth of a turn
            let spun = SPINNER_FRAMES / 8;
            let mut spinner_strip = strip_of(8);
            let mut next_status = 0;
            for icon in Icon::all() {
                if !matches!(icon, Icon::Spinner(n) if n % spun != 0) {
                    let name = format!("{icon:?}").to_lowercase().replace('(', "_").replace(')', "");
                    on_bg(icon, None).save_png(out.join(format!("{name}.png"))).unwrap();
                }
                let (s, i) = match icon {
                    Icon::Spinner(n) if n % spun == 0 => (&mut spinner_strip, u32::from(n / spun)),
                    // Only the strip for the rest of the turn
                    Icon::Spinner(_) => continue,
                    Icon::Pairing(n) => (&mut strip, u32::from(n)),
                    _ => {
                        next_status += 1;
                        (&mut status_strip, next_status - 1)
                    }
                };
                for (row, &badge) in badges.iter().enumerate().filter(|(_, b)| b.is_none() || icon.takes_badge()) {
                    let (x, y) = ((i * (size + GAP)) as i32, (row as u32 * (size + GAP)) as i32);
                    s.draw_pixmap(
                        x,
                        y,
                        on_bg(icon, badge).as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                }
            }
            status_strip.save_png(out.join("status_strip.png")).unwrap();
            // The menu's always light, so its marks are the same either way: the signal bars, then
            // a critical battery's warning, both frames
            if theme == "light" {
                let marks = SIGNALS.map(Mark::Signal).into_iter().chain([Mark::Critical, Mark::CriticalDim]);
                let n = SIGNALS.len() as u32 + 2;
                let mut mark_strip = Pixmap::new(n * size + (n - 1) * GAP, size).unwrap();
                mark_strip.fill(resvg::tiny_skia::Color::from_rgba8(0x80, 0x80, 0x80, 0xFF));
                let menu_bg = resvg::tiny_skia::Color::from_rgba8(0xF9, 0xF9, 0xF9, 0xFF);
                for (i, m) in marks.enumerate() {
                    let mut pm = Pixmap::new(size, size).unwrap();
                    pm.fill(menu_bg);
                    let fg = rasterize(&mark_svg(m), size).unwrap();
                    pm.draw_pixmap(0, 0, fg.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    let name = format!("{m:?}").to_lowercase().replace("signal(", "signal_").replace(')', "");
                    pm.save_png(out.join(format!("{name}.png"))).unwrap();
                    let x = (i as u32 * (size + GAP)) as i32;
                    mark_strip.draw_pixmap(x, 0, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
                mark_strip.save_png(out.join("mark_strip.png")).unwrap();
            }
            strip.save_png(out.join("pairing_strip.png")).unwrap();
            spinner_strip.save_png(out.join("spinner_strip.png")).unwrap();
        }
    }

    const SIGNALS: [Signal; 4] = [Signal::Dropping, Signal::Low, Signal::Fair, Signal::Good];

    #[test]
    fn critical_warning_is_red_then_red_with_a_black_mark() {
        let lit = mark_svg(Mark::Critical);
        assert!(!lit.contains("#000000") && lit.matches(LIGHT.red).count() == 3, "all three paths red");
        let dim = mark_svg(Mark::CriticalDim);
        assert_eq!(
            (dim.matches(LIGHT.red).count(), dim.matches(LIGHT.stroke).count()),
            (1, 2),
            "outline red, mark black"
        );
        for m in [Mark::Critical, Mark::CriticalDim] {
            let pm = rasterize(&mark_svg(m), 16).unwrap();
            assert!(pm.pixels().iter().any(|px| px.alpha() > 0), "{m:?} is blank");
        }
    }

    /// Each bar's color, left to right
    fn bars(s: Signal) -> Vec<String> {
        let svg = signal_svg(s);
        let places = signal_bars().1.into_iter().map(|(place, _)| place);
        let mut bars: Vec<(usize, String)> = places
            .zip(svg.split("<path").skip(1))
            .map(|(place, p)| (place, p.split("stroke=\"").nth(1).unwrap().split('"').next().unwrap().to_owned()))
            .collect();
        bars.sort();
        bars.into_iter().map(|(_, c)| c).collect()
    }

    #[test]
    fn signal_bars_light_from_the_left() {
        let (u, r, y, g) = (SIGNAL_UNLIT, LIGHT.red, LIGHT.yellow, LIGHT.green);
        assert_eq!(bars(Signal::Dropping), [u, u, u]);
        assert_eq!(bars(Signal::Low), [r, u, u]);
        assert_eq!(bars(Signal::Fair), [y, y, u]);
        assert_eq!(bars(Signal::Good), [g, g, g]);
        for s in SIGNALS {
            let pm = rasterize(&signal_svg(s), 16).unwrap();
            assert!(pm.pixels().iter().any(|px| px.alpha() > 0), "{s:?} is blank");
        }
    }

    #[test]
    fn badges_drop_the_ring() {
        for n in BADGES {
            let svg = badge_svg(n, &DARK).unwrap();
            assert!(!svg.contains("A104") && !svg.contains("a88"), "{n}: the ring's still there");
            assert!(svg.contains(&format!("<circle fill=\"{}\"", DARK.blue)) && !svg.contains("opacity"));
            assert!(svg.contains(DARK.stroke) && !svg.contains("#000000"));
            let pm = rasterize(&svg, 32).unwrap();
            assert!(pm.pixels().iter().any(|px| px.alpha() > 0), "{n} is blank");
        }
        assert!(badge_svg(1, &DARK).is_none() && badge_svg(5, &DARK).is_none());
        let pm = Icon::Full.badged(&DARK, 32, 32, Some(3)).unwrap();
        // The disc is in the corner, where the plain icon has none of its blue
        let blue = |pm: &Pixmap| pm.pixel(28, 28).unwrap().demultiply().blue();
        assert!(blue(&pm) > 0xC0 && blue(&Icon::Full.pixmap(&DARK, 32).unwrap()) < 0x40);
    }

    /// Whether anything's drawn in the `x` by `y` ranges of `pm`
    fn inked(pm: &Pixmap, x: std::ops::Range<u32>, y: std::ops::Range<u32>) -> bool {
        y.flat_map(|y| x.clone().map(move |x| (x, y))).any(|(x, y)| pm.pixel(x, y).unwrap().alpha() > 0)
    }

    #[test]
    fn a_smaller_drawing_keeps_a_margin() {
        for icon in [Icon::Full, Icon::Pairing(3), Icon::Spinner(2), Icon::Disconnected] {
            let pm = icon.badged(&DARK, 48, 36, None).unwrap();
            assert!(inked(&pm, 6..42, 6..42), "{icon:?} is drawn");
            // Centered: nothing outside the middle 36 square
            assert!(!inked(&pm, 0..48, 0..6) && !inked(&pm, 0..48, 42..48), "{icon:?} above or below");
            assert!(!inked(&pm, 0..6, 0..48) && !inked(&pm, 42..48, 0..48), "{icon:?} to the side");
            // The drawing's the 36 one, moved over
            let small = icon.badged(&DARK, 36, 36, None).unwrap();
            assert_eq!(pm.pixel(24, 24), small.pixel(18, 18), "{icon:?}");
        }
    }

    #[test]
    fn a_badge_keeps_its_size_by_a_smaller_drawing() {
        let side = (48.0 * BADGE_SCALE).round() as u32;
        for icon in [Icon::Full, Icon::Pairing(3), Icon::Spinner(2)] {
            let small = icon.badged(&DARK, 48, 36, Some(2)).unwrap();
            let full = icon.badged(&DARK, 48, 48, Some(2)).unwrap();
            // The badge, 30 across in the corner, is the same either way, away from the drawing
            for (x, y) in [(40, 40), (47 - side / 2, 47), (47, 47 - side / 2), (44, 26)] {
                assert_eq!(small.pixel(x, y), full.pixel(x, y), "{icon:?} at {x},{y}");
            }
            assert!(!inked(&small, 36..48, 0..48 - side), "{icon:?} right of its square, above the badge");
            match icon.is_battery() {
                // To the left, at its usual height: nothing above or below its square
                true => assert!(!inked(&small, 0..48, 0..6) && !inked(&small, 0..48 - side, 42..48), "{icon:?}"),
                // Up in the top left
                false => assert!(!inked(&small, 0..48 - side, 36..48), "{icon:?} below its square"),
            }
            assert!(inked(&small, 0..18, 0..36), "{icon:?} is drawn");
        }
    }

    #[test]
    fn animations_step_and_keep_their_frame() {
        assert_eq!(Icon::Charging.next_frame(), Some(Icon::ChargingLit));
        assert_eq!(Icon::ChargingLit.next_frame(), Some(Icon::Charging));
        assert_eq!(Icon::Disconnected.next_frame(), Some(Icon::DisconnectedRed));
        assert_eq!(Icon::DisconnectedRed.next_frame(), Some(Icon::Disconnected));
        assert_eq!(Icon::keep_frame(Icon::Disconnected, Some(Icon::DisconnectedRed)), Icon::DisconnectedRed);
        let red = Icon::DisconnectedRed.svg(&LIGHT);
        assert!(red.contains(LIGHT.red) && !red.contains(LIGHT.stroke));
        assert_eq!(Icon::Pairing(PAIRING_FRAMES - 1).next_frame(), Some(Icon::Pairing(0)));
        assert_eq!(Icon::Full.next_frame(), None);
        assert_eq!(Icon::keep_frame(Icon::Charging, Some(Icon::ChargingLit)), Icon::ChargingLit);
        assert_eq!(Icon::keep_frame(Icon::Pairing(0), Some(Icon::Pairing(3))), Icon::Pairing(3));
        assert_eq!(Icon::keep_frame(Icon::Full, Some(Icon::ChargingLit)), Icon::Full);
        // Stroke outline in both flash frames; only the bolt flashes green
        let still = Icon::Charging.svg(&DARK);
        assert!(still.contains(DARK.stroke) && !still.contains(DARK.green));
        let lit = Icon::ChargingLit.svg(&DARK);
        assert_eq!(lit.matches(DARK.green).count(), 1);
        assert!(lit.contains(DARK.stroke));
        // Same for the critical mark, on a red outline
        assert_eq!(Icon::Critical.next_frame(), Some(Icon::CriticalDim));
        assert_eq!(Icon::CriticalDim.next_frame(), Some(Icon::Critical));
        assert_eq!(Icon::keep_frame(Icon::Critical, Some(Icon::CriticalDim)), Icon::CriticalDim);
        let dim = Icon::CriticalDim.svg(&LIGHT);
        assert_eq!(dim.matches(LIGHT.red).count(), 1);
        assert_eq!(dim.matches(LIGHT.stroke).count(), 2);
    }

    #[test]
    fn spinner_turns_in_place() {
        assert_eq!(SPINNER_FRAMES, 24);
        assert_eq!(Icon::Spinner(SPINNER_FRAMES - 1).next_frame(), Some(Icon::Spinner(0)));
        assert_eq!(Icon::keep_frame(Icon::Spinner(0), Some(Icon::Spinner(9))), Icon::Spinner(9));
        // Half a turn on is the same drawing upside down, so it stays put rather than drifting
        let a = Icon::Spinner(0).pixmap(&DARK, 32).unwrap();
        let b = Icon::Spinner(SPINNER_FRAMES / 2).pixmap(&DARK, 32).unwrap();
        let alpha = |pm: &Pixmap, x: u32, y: u32| pm.pixel(x, y).unwrap().alpha();
        let off = (0..32)
            .flat_map(|y| (0..32).map(move |x| (x, y)))
            .filter(|&(x, y)| alpha(&a, x, y).abs_diff(alpha(&b, 31 - x, 31 - y)) > 64)
            .count();
        assert!(off < 8, "{off} pixels off");
    }

    #[test]
    fn icon_choice() {
        let b = |band, charging, critical| Battery { charging, band, reading: 0xD500, critical, must_charge: false };
        assert_eq!(Icon::of(&b(100, false, false), 80), Icon::Full);
        assert_eq!(Icon::of(&b(75, false, false), 92), Icon::Full);
        assert_eq!(Icon::of(&b(50, false, false), 50), Icon::Medium);
        assert_eq!(Icon::of(&b(25, false, false), 30), Icon::Low);
        assert_eq!(Icon::of(&b(10, false, false), 12), Icon::Empty);
        assert_eq!(Icon::of(&b(0, false, false), 3), Icon::Empty);
        assert_eq!(Icon::of(&b(10, false, true), 2), Icon::Critical);
        assert_eq!(Icon::of(&b(10, true, true), 2), Icon::Charging);
        assert_eq!(Icon::of(&b(100, true, false), 99), Icon::Charging);
        assert_eq!(Icon::of(&b(100, true, false), 100), Icon::Charged);
        // Off the charger, 100% is just full
        assert_eq!(Icon::of(&b(100, false, false), 100), Icon::Full);
        let critical = Icon::Critical.svg(&DARK);
        assert!(critical.contains(DARK.red) && !critical.contains(DARK.stroke));
        let charged = Icon::Charged.svg(&DARK);
        assert!(charged.contains(DARK.green) && !charged.contains(DARK.stroke));
    }
}
