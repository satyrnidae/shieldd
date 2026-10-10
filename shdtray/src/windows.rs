//! The Windows tray: an icon for the controller's battery (mailbox 01, shieldd's controller.rs),
//! with a notification when it goes critical and another when it's kicked for being too flat to
//! use. Shown as long as shieldd runs, disconnected or not.
//!
//! Battery icons are Ikono Bold Line Icons, turned upright; the disconnected and pairing ones
//! are from Clarity. All from shdtray/res/, MIT, see THIRD_PARTY.md. Rendered at the tray's
//! size and colored for the taskbar's light or dark theme.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
    MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT, SND_SYSTEM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::EM_LIMITTEXT;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForSystem, GetSystemMetricsForDpi,
    MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_NOSOUND, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DS_CENTER,
    DS_MODALFRAME, DS_SETFONT, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DialogBoxIndirectParamW,
    DispatchMessageW, ES_AUTOHSCROLL, EndDialog, EndMenu, EnumThreadWindows, GetClassNameW, GetCursorPos,
    GetDlgItemTextW, GetMessageW, HICON, HMENU, ICONINFO, IDCANCEL, IDOK, KillTimer, MB_DEFBUTTON2, MB_ICONINFORMATION,
    MB_ICONWARNING, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MENU_ITEM_FLAGS, MENUITEMINFOW, MESSAGEBOX_RESULT,
    MESSAGEBOX_STYLE, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SendDlgItemMessageW,
    SetDlgItemTextW, SetForegroundWindow, SetMenuItemInfoW, SetTimer, TPM_RETURNCMD, TrackPopupMenu, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_INITDIALOG, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::battery::{Battery, Gauge};
use crate::tr;
use crate::{About, Command, Connected, MAX_CONTROLLERS, Shunned, Signal, Status};
use shdlib::names::MAX_NAME;
use shdnet::mac::MacFmt;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

//region Icons

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
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
/// Each frame is a call into Explorer, so no more than it takes to look smooth
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
const FLASH_FRAME: Duration = Duration::from_millis(800);
/// How long a flash runs before the icon holds still: the disconnected one at startup and each
/// time a controller leaves on its own, rather than sent away; the charging bolt each time
/// charging starts
const FLASH_FOR: Duration = Duration::from_secs(5);
/// How many controllers the corner badge can count: one has none, and four is all there can be
const BADGES: std::ops::RangeInclusive<u8> = 2..=MAX_CONTROLLERS as u8;
/// The badge's side, as a share of the icon's
const BADGE_SCALE: f32 = 5.0 / 8.0;
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
    fn all() -> impl Iterator<Item = Icon> {
        Icon::FIXED.into_iter().chain((0..SPINNER_FRAMES).map(Icon::Spinner))
    }

    /// For `b`, with `pct` the percent shown
    fn of(b: &Battery, pct: u8) -> Icon {
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
            Icon::Charging => (include_str!("../res/battery-charging-svgrepo-com.svg"), Accent::None),
            Icon::ChargingLit => (include_str!("../res/battery-charging-svgrepo-com.svg"), Accent::Green),
            Icon::Charged => (include_str!("../res/battery-full-svgrepo-com.svg"), Accent::Green),
            Icon::Full => (include_str!("../res/battery-full-svgrepo-com.svg"), Accent::Green),
            Icon::Medium => (include_str!("../res/battery-medium-svgrepo-com.svg"), Accent::Yellow),
            Icon::Low => (include_str!("../res/battery-low-svgrepo-com.svg"), Accent::Red),
            Icon::Empty => (include_str!("../res/battery-empty-svgrepo-com.svg"), Accent::Red),
            Icon::Critical => (include_str!("../res/battery-warning-svgrepo-com.svg"), Accent::Red),
            // Its color is on the root element, before any path; all of it is outline
            Icon::Disconnected => (include_str!("../res/disconnected-svgrepo-com.svg"), Accent::None),
            Icon::DisconnectedRed => (include_str!("../res/disconnected-svgrepo-com.svg"), Accent::Red),
            Icon::CriticalDim => (include_str!("../res/battery-warning-svgrepo-com.svg"), Accent::None),
            // Colored by pairing_frame instead
            Icon::Pairing(_) => (include_str!("../res/wifi-svgrepo-com.svg"), Accent::None),
            // Turned by pixmap
            Icon::Spinner(_) => (include_str!("../res/loading-spinner-svgrepo-com.svg"), Accent::None),
        }
    }

    /// The animation frame after this one, for the animated icons
    fn next_frame(self) -> Option<Icon> {
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
    fn frame_time(self) -> Duration {
        match self {
            Icon::Pairing(_) => PAIRING_FRAME,
            Icon::Spinner(_) => SPINNER_FRAME,
            _ => FLASH_FRAME,
        }
    }

    /// `want`, or the frame of it already on screen, so an update doesn't restart the animation
    fn keep_frame(want: Icon, shown: Option<Icon>) -> Icon {
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
    fn takes_badge(self) -> bool {
        !matches!(self, Icon::Disconnected | Icon::DisconnectedRed)
    }

    /// As pixmap, with the count badge in the lower right corner, if any
    fn badged(self, p: &Palette, size: u32, badge: Option<u8>) -> Result<Pixmap> {
        let Some(n) = badge else { return self.pixmap(p, size) };
        // A battery's narrow enough to step aside to the left, out from under the badge
        let mut pm = match self.is_battery() {
            true => fit(&self.svg(p), size, false),
            false => self.pixmap(p, size),
        }?;
        let svg = badge_svg(n, p).with_context(|| format!("no badge for {n}"))?;
        let side = ((size as f32 * BADGE_SCALE).round() as u32).max(1);
        let b = rasterize(&svg, side)?;
        let at = (size - side) as i32;
        pm.draw_pixmap(at, at, b.as_ref(), &resvg::tiny_skia::PixmapPaint::default(), Transform::identity(), None);
        Ok(pm)
    }
}

/// The count badge for `n` controllers, from Phosphor's duotone digits: the disc in blue at
/// full strength, the ring around it dropped, and the number in the stroke color
fn badge_svg(n: u8, p: &Palette) -> Option<String> {
    let svg = match n {
        2 => include_str!("../res/number-circle-two-duotone-svgrepo-com.svg"),
        3 => include_str!("../res/number-circle-three-duotone-svgrepo-com.svg"),
        4 => include_str!("../res/number-circle-four-duotone-svgrepo-com.svg"),
        _ => return None,
    };
    let (head, number) = svg.split_once(BADGE_RING_END)?;
    let d = head.rfind("d=\"")? + 3;
    let svg = format!("{}M128,216{number}", &head[..d]);
    let disc = format!("<circle fill=\"{}\" ", p.blue);
    Some(svg.replace("<g opacity=\"0.2\">", "<g>").replacen("<circle ", &disc, 1).replace("#000000", p.stroke))
}

/// The menu's signal bars, Ikono's graph-asc
const SIGNAL_SVG: &str = include_str!("../res/graph-asc-svgrepo-com.svg");
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
struct Palette {
    stroke: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    /// The pairing animation
    blue: &'static str,
}

/// Only the stroke and yellow differ by theme; the darker reds and greens were too close to black
/// to flash against it
const LIGHT: Palette =
    Palette { stroke: "#000000", green: "#44AA00", yellow: "#D4AA00", red: "#AA0000", blue: "#0066FF" };
const DARK: Palette =
    Palette { stroke: "#FFFFFF", green: "#44AA00", yellow: "#FFD42A", red: "#AA0000", blue: "#0066FF" };

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

/// True when the taskbar uses the light theme. Windows keeps it apart from the apps' theme.
fn light_taskbar() -> bool {
    let mut v = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: v is a u32 and len says so
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&raw mut v as *mut core::ffi::c_void),
            Some(&mut len),
        )
    };
    // Missing means pre-1903 Windows, whose taskbar is dark
    err.is_ok() && v != 0
}

/// The drawing itself, stroke included, fit to a `size` square and centered; the canvas has
/// margins
fn rasterize(svg: &str, size: u32) -> Result<Pixmap> {
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

fn render(pm: &Pixmap) -> Result<HICON> {
    let size = pm.width();
    // Icons take straight-alpha BGRA; tiny-skia has premultiplied RGBA
    let bgra: Vec<u8> = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.blue(), c.green(), c.red(), c.alpha()]
        })
        .collect();
    let side = size as i32;
    let color = dib(side, &bgra)?;
    // SAFETY: both bitmaps are deleted once the icon has its own copies
    unsafe {
        // All-zero AND mask: the alpha channel does the masking
        let mask = CreateBitmap(side, side, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.context("creating icon")
    }
}

/// A menu item's image: a top-down 32-bit DIB, premultiplied BGRA as menus want it. The caller
/// deletes it.
fn menu_bitmap(pm: &Pixmap) -> Result<HBITMAP> {
    let bgra: Vec<u8> = pm.pixels().iter().flat_map(|p| [p.blue(), p.green(), p.red(), p.alpha()]).collect();
    dib(pm.width() as i32, &bgra)
}

/// A `side` square top-down 32-bit DIB section holding `bgra`
fn dib(side: i32, bgra: &[u8]) -> Result<HBITMAP> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: side,
            biHeight: -side, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(bgra.len(), (side * side * 4) as usize);
    // SAFETY: the DIB is side x side x 4 bytes, exactly bgra's length
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).context("bitmap")?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        Ok(bmp)
    }
}

//endregion

//region Tray

enum Msg {
    Status(Status),
    /// Something went wrong that the user should hear about: title, text
    Problem(String, String),
    Quit,
}

/// The tray icon and its menu, on their own thread. Shown from start to drop.
pub struct Tray {
    tx: Sender<Msg>,
    commands: Receiver<Command>,
    thread: Option<JoinHandle<()>>,
    last: Option<Status>,
}

impl Tray {
    pub fn start(about: About) -> Tray {
        let _ = ABOUT.set(about);
        let (tx, rx) = mpsc::channel();
        let (cmd_tx, commands) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                if let Err(e) = run(rx, cmd_tx) {
                    warn!("tray icon: {e:#}");
                }
            })
            .ok();
        let mut tray = Tray { tx, commands, thread, last: None };
        tray.update(Status::default());
        tray
    }

    /// Only changes go through
    pub fn update(&mut self, s: Status) {
        if self.last.as_ref() != Some(&s) {
            self.last = Some(s.clone());
            let _ = self.tx.send(Msg::Status(s));
            wake();
        }
    }

    /// A notification without the alarm, for a menu command that failed
    pub fn problem(&self, title: &str, text: &str) {
        let _ = self.tx.send(Msg::Problem(title.into(), text.into()));
        wake();
    }

    /// Menu picks since the last call
    pub fn commands(&self) -> Vec<Command> {
        self.commands.try_iter().collect()
    }
}

/// Windows' own Device Connect or Device Disconnect sound, as for a USB device. Follows the sound
/// scheme: silent if it has none, no fallback ding.
pub fn device_sound(connected: bool) {
    let alias = if connected { w!("DeviceConnect") } else { w!("DeviceDisconnect") };
    // SAFETY: a static alias; async, so nothing of ours is used after it returns
    let _ = unsafe { PlaySoundW(alias, None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM) };
}

/// The user's display languages as BCP 47 tags, most preferred first. None if Windows won't say.
pub(crate) fn ui_languages() -> Vec<String> {
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: a size query, then a buffer of that size; both counts are ours
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len).is_err() {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(PWSTR(buf.as_mut_ptr())), &mut len).is_err()
        {
            return Vec::new();
        }
        // NUL between each, two at the end
        buf.split(|&c| c == 0).filter(|t| !t.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

/// From Tray::start, for the menu's title and the About box
static ABOUT: std::sync::OnceLock<About> = std::sync::OnceLock::new();

fn about_info() -> About {
    ABOUT.get().copied().unwrap_or(About { version: "?", authors: "", built: "?" })
}

/// Has the tray thread look at the channel now, rather than at its next tick
fn wake() {
    let hwnd = TRAY_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: a stale handle just fails the post
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_WAKE, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        // An open message box or menu runs its own loop and never sees Quit, but it still
        // dispatches this
        let hwnd = TRAY_HWND.load(Ordering::Acquire);
        if hwnd != 0 {
            // SAFETY: a stale handle just fails the post
            let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How often to look for a taskbar theme change
const THEME_EVERY: Duration = Duration::from_secs(5);
/// A kicked controller may come back a few times before it gives up; say so only once a while
const MUST_CHARGE_EVERY: Duration = Duration::from_secs(600);
/// The icon's callback message
const WM_TRAY: u32 = WM_APP + 1;
/// Close whatever's open, see Tray::drop
const WM_SHUTDOWN: u32 = WM_APP + 2;
/// Something's waiting on the channel, see wake
const WM_WAKE: u32 = WM_APP + 3;
/// State::tick's timer, and the longest it waits with nothing animating
const TICK_TIMER: usize = 1;
const IDLE_TICK: Duration = Duration::from_millis(100);

/// A menu item, or a submenu with its items
#[derive(Debug, Clone, PartialEq)]
struct Item {
    flags: MENU_ITEM_FLAGS,
    /// Ignored for a submenu
    id: usize,
    text: String,
    sub: Vec<Item>,
    /// Signal bars beside the text
    signal: Option<Signal>,
}

impl Item {
    fn new(flags: MENU_ITEM_FLAGS, id: usize, text: impl Into<String>) -> Item {
        Item { flags, id, text: text.into(), sub: Vec::new(), signal: None }
    }

    fn separator() -> Item {
        Item::new(MF_SEPARATOR, 0, "")
    }
}
/// The tray window, for Tray::drop; one per process
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// Menu item IDs; 0 is TrackPopupMenu's "nothing picked"
mod id {
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
    pub const VBAN: usize = 2;
    pub const DISCONNECT: usize = 3;

    pub fn of(index: usize, action: usize) -> usize {
        CONTROLLER + index * PER_CONTROLLER + action
    }

    /// A controller's index and action, from a pick
    pub fn controller(picked: usize) -> Option<(usize, usize)> {
        let n = picked.checked_sub(CONTROLLER)?;
        Some((n / PER_CONTROLLER, n % PER_CONTROLLER))
    }
}

/// The menu closed with the cursor on the icon: the click that closed it arrives as a tray
/// click within this long, if at all
const CLOSING_CLICK_WAIT: Duration = Duration::from_secs(1);

// The window procedure flags what happened, or runs tick with State at hand. Everything the
// tray does runs off window messages, which an open menu or message box still dispatches.
thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static MENU_WANTED: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static READD_WANTED: Cell<bool> = const { Cell::new(false) };
    /// Shutting down: no more menus or boxes
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    /// The open menu's items, so tick can tell when they've gone stale
    static MENU_SHOWN: RefCell<Option<Vec<Item>>> = const { RefCell::new(None) };
    /// The open menu was closed to show new items; open it again
    static MENU_REOPEN: Cell<bool> = const { Cell::new(false) };
    /// The menu closed without a pick, with the cursor on the icon, at this time
    static CLOSED_ON_ICON: Cell<Option<Instant>> = const { Cell::new(None) };
    /// In menu(): the menu, or a box it opened, is up
    static MENU_BUSY: Cell<bool> = const { Cell::new(false) };
    /// The rename dialog's text, in and out
    static RENAME_TEXT: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` on the tray's state, unless it's already in use further up the stack
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TRAY && matches!(lp.0 as u32 & 0xFFFF, WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU) {
        if MENU_BUSY.get() {
            // A box from the menu is up, maybe behind something; that's what to show
            raise_modals();
        } else if CLOSED_ON_ICON.take().is_some_and(|t| t.elapsed() < CLOSING_CLICK_WAIT) {
            // The click that closed the menu, arriving after it: that's all it was for
        } else {
            MENU_WANTED.set(true);
        }
    } else if msg == WM_SHUTDOWN {
        QUITTING.set(true);
        close_modals();
        // Quit is on the channel already
        with_state(State::tick);
    } else if (msg == WM_TIMER && wp.0 == TICK_TIMER) || msg == WM_WAKE {
        // Busy further up the stack, e.g. inside Shell_NotifyIcon: the next tick catches up
        with_state(State::tick);
    } else if msg != 0 && msg == TASKBAR_CREATED.get() {
        // Explorer restarted and forgot every icon
        READD_WANTED.set(true);
        with_state(State::tick);
    }
    // SAFETY: the arguments we were called with
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Brings any message box or dialog of this thread to the front
fn raise_modals() {
    extern "system" fn raise(h: HWND, _: LPARAM) -> BOOL {
        let mut class = [0u16; 16];
        // SAFETY: h is a window of this thread, from the enumeration
        unsafe {
            let n = GetClassNameW(h, &mut class) as usize;
            if String::from_utf16_lossy(&class[..n]) == "#32770" {
                let _ = SetForegroundWindow(h);
            }
        }
        BOOL(1)
    }
    // SAFETY: the callback only touches windows it's handed
    let _ = unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(raise), LPARAM(0)) };
}

/// Whether the cursor is on our tray icon
fn cursor_on_icon(hwnd: HWND) -> bool {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let mut at = POINT::default();
    // SAFETY: id and at outlive the calls
    unsafe {
        let Ok(r) = Shell_NotifyIconGetRect(&id) else { return false };
        GetCursorPos(&mut at).is_ok() && (r.left..r.right).contains(&at.x) && (r.top..r.bottom).contains(&at.y)
    }
}

/// Cancels any message box and closes any menu open on this thread
fn close_modals() {
    extern "system" fn cancel(h: HWND, _: LPARAM) -> BOOL {
        let mut class = [0u16; 16];
        // SAFETY: h is a window of this thread, from the enumeration
        unsafe {
            let n = GetClassNameW(h, &mut class) as usize;
            // The dialog class, which message boxes use
            if String::from_utf16_lossy(&class[..n]) == "#32770" {
                let _ = EndDialog(h, IDCANCEL.0 as isize);
            }
        }
        BOOL(1)
    }
    // SAFETY: the callback only touches windows it's handed
    unsafe {
        let _ = EndMenu();
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(cancel), LPARAM(0));
    }
}

/// Stops the message loop in run, and anything modal in the way
fn quit() {
    QUITTING.set(true);
    close_modals();
    // SAFETY: plain call, on the tray thread
    unsafe { PostQuitMessage(0) };
}

/// What a notification looks and sounds like
#[derive(Debug, Clone, Copy)]
enum Notice {
    Info,
    Warning,
    /// The error icon and the critical battery alarm
    Alarm,
}

/// One controller's battery, as the tray follows it
#[derive(Debug, Default)]
struct Watch {
    /// Its place in the order controllers connected, while it is
    connection: Option<u64>,
    gauge: Gauge,
    /// The gauge's last answer
    pct: Option<u8>,
    critical_alerted: bool,
    /// Told it's low, this discharge
    low_alerted: bool,
    /// Last refresh found it on the charger and full
    was_charged: bool,
    /// Last must-charge notification
    must_charge_told: Option<Instant>,
    /// Last refresh found it on the charger
    was_charging: bool,
    /// Its charging bolt flashes until then
    charge_flash_until: Option<Instant>,
}

struct State {
    hwnd: HWND,
    rx: Receiver<Msg>,
    light: bool,
    last_theme: Instant,
    /// Every icon, with each count badge it takes
    icons: Vec<(Icon, Option<u8>, HICON)>,
    /// The count badge to show, with more than one controller connected
    badge: Option<u8>,
    /// What's on screen, if the icon is shown
    shown: Option<Icon>,
    status: Status,
    /// Every controller seen this run, by MAC
    watches: HashMap<[u8; 6], Watch>,
    /// The controller the battery icon shows: the lowest, discharging first
    focus: Option<[u8; 6]>,
    /// The next controller to connect gets this as its connection
    connections: u64,
    /// When the animation on screen last moved
    frame_at: Instant,
    /// Last refresh found nothing connected, pairing or joining
    was_disconnected: bool,
    /// A controller was connected since the last time nothing was
    had_controller: bool,
    /// No refresh yet
    first: bool,
    /// The disconnected icon flashes until then
    flash_until: Option<Instant>,
    /// What the battery icon settles on once a flash is over
    settled: Option<Icon>,
    commands: Sender<Command>,
}

fn run(rx: Receiver<Msg>, commands: Sender<Command>) -> Result<()> {
    // shieldd isn't DPI-aware, so Windows would hand out 100% sizes and stretch the result.
    // This thread is, so icons render at the display's real tray size.
    // SAFETY: affects only this thread
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // A hidden top-level window rather than a message-only one: the menu needs a window that
    // can come to the foreground, and only top-level ones hear TaskbarCreated
    // SAFETY: class and window live until the end of this function; wndproc is 'static
    let hwnd = unsafe {
        let instance = GetModuleHandleW(None).context("module handle")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("shieldd tray"),
            ..Default::default()
        };
        RegisterClassW(&class);
        TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("shieldd tray"),
            w!("shieldd"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .context("tray window")?;
    let mut st = State {
        hwnd,
        rx,
        light: light_taskbar(),
        last_theme: Instant::now(),
        icons: Vec::new(),
        badge: None,
        shown: None,
        status: Status::default(),
        watches: HashMap::new(),
        focus: None,
        connections: 0,
        frame_at: Instant::now(),
        was_disconnected: false,
        had_controller: false,
        first: true,
        flash_until: None,
        settled: None,
        commands,
    };
    st.load_icons()?;
    STATE.set(Some(st));
    // Only now: a wake before STATE is set would find nothing to tick
    TRAY_HWND.store(hwnd.0 as isize, Ordering::Release);
    with_state(State::tick);
    let mut msg = MSG::default();
    // SAFETY: msg outlives the calls; 0 is WM_QUIT, -1 an error
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe { DispatchMessageW(&msg) };
        if MENU_WANTED.replace(false) && !QUITTING.get() {
            menu(hwnd);
        }
    }
    TRAY_HWND.store(0, Ordering::Release);
    // Only past the loop: EndMenu doesn't wait, so the menu may still have been up in quit
    for (.., b) in SIGNAL_BITMAPS.take() {
        // SAFETY: made by signal_bitmap; menu returned before the loop did
        let _ = unsafe { DeleteObject(b.into()) };
    }
    if let Some(mut st) = STATE.take() {
        st.hide();
        st.free_icons();
    }
    // SAFETY: created above on this thread
    unsafe {
        let _ = KillTimer(Some(hwnd), TICK_TIMER);
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

/// Shows the menu at the cursor and acts on the pick. Reopened in place whenever tick finds
/// its items stale, so it follows pairing and controllers coming and going.
fn menu(hwnd: HWND) {
    MENU_BUSY.set(true);
    menu_and_boxes(hwnd);
    MENU_BUSY.set(false);
}

fn menu_and_boxes(hwnd: HWND) {
    let mut at = POINT::default();
    // SAFETY: at outlives the call
    let _ = unsafe { GetCursorPos(&mut at) };
    let (picked, s, commands) = loop {
        let Some((items, s, commands)) = with_state(|st| (st.menu_items(), st.status.clone(), st.commands.clone()))
        else {
            return;
        };
        MENU_REOPEN.set(false);
        MENU_SHOWN.set(Some(without_signal(&items)));
        let picked = track(hwnd, &items, at);
        MENU_SHOWN.set(None);
        if picked == 0 && MENU_REOPEN.replace(false) && !QUITTING.get() {
            continue;
        }
        MENU_REOPEN.set(false);
        if picked == 0 && cursor_on_icon(hwnd) {
            CLOSED_ON_ICON.set(Some(Instant::now()));
        }
        break (picked, s, commands);
    };
    let cmd = match (picked, id::controller(picked)) {
        _ if QUITTING.get() => return,
        (id::PAIR, _) if confirm_pair(hwnd) => Command::Pair,
        (id::RECONFIGURE, _) if confirm_reconfigure(hwnd) => Command::Reconfigure,
        (id::EXIT, _) => Command::Exit,
        (id::ABOUT, _) => return about(hwnd),
        (_, Some((n, action))) => {
            let Some(c) = s.controllers.get(n) else { return };
            match action {
                id::IDENTIFY => Command::Identify(c.mac),
                id::RENAME => {
                    let Some(name) = rename(hwnd, c) else { return };
                    Command::Rename(c.mac, name)
                }
                id::VBAN => Command::Vban(c.mac, !c.vban),
                id::DISCONNECT => Command::Disconnect(c.mac),
                _ => return,
            }
        }
        _ => return,
    };
    debug!(?cmd, "tray menu");
    // The first one on starts the streams; say where they are
    let vban_on = matches!(cmd, Command::Vban(_, true)) && !s.controllers.iter().any(|c| c.vban);
    let _ = commands.send(cmd);
    if vban_on && !QUITTING.get() {
        let text = tr!(
            "vban-on",
            mic_stream = shdlib::vban::MIC_STREAM,
            mic_port = shdlib::vban::MIC_PORT,
            pad_stream = shdlib::vban::PAD_STREAM,
            pad_port = shdlib::vban::PAD_PORT,
        );
        message(hwnd, &text, &tr!("vban-on-title"), MB_OK | MB_ICONINFORMATION);
    }
}

thread_local! {
    /// Signal bars as menu images, by level and size, made on first use; freed at quit
    static SIGNAL_BITMAPS: RefCell<Vec<(Signal, u32, HBITMAP)>> = const { RefCell::new(Vec::new()) };
}

/// The signal bars for `s` as a `size` square menu image, rendered the first time
fn signal_bitmap(s: Signal, size: u32) -> Option<HBITMAP> {
    let cached = SIGNAL_BITMAPS.with_borrow(|c| c.iter().find(|(m, n, _)| (*m, *n) == (s, size)).map(|e| e.2));
    if cached.is_some() {
        return cached;
    }
    let b = rasterize(&signal_svg(s), size).and_then(|pm| menu_bitmap(&pm));
    let b = b.inspect_err(|e| warn!("signal bars: {e:#}")).ok()?;
    SIGNAL_BITMAPS.with_borrow_mut(|c| c.push((s, size, b)));
    Some(b)
}

/// A small icon's side at `dpi`: the tray icon's, and the menu's images
fn small_icon_size(dpi: u32) -> u32 {
    // SAFETY: plain query
    unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.max(16) as u32
}

/// `items` without their signal bars, for telling whether an open menu's stale
fn without_signal(items: &[Item]) -> Vec<Item> {
    items.iter().map(|i| Item { signal: None, sub: without_signal(&i.sub), ..i.clone() }).collect()
}

/// Pops up `items` at `at` until something's picked or it's closed; 0 for nothing
fn track(hwnd: HWND, items: &[Item], at: POINT) -> usize {
    // The image size for the monitor it opens on, which may not be at the system's DPI
    // SAFETY: plain queries
    let dpi = unsafe {
        let (mut x, mut y) = (0, 0);
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) {
            Ok(()) => x,
            Err(_) => GetDpiForSystem(),
        }
    };
    let size = small_icon_size(dpi);
    // SAFETY: the menus are ours and destroyed below, submenus with the top; the strings outlive
    // AppendMenuW, which copies them; the bitmaps are cached until quit
    unsafe {
        let Some(m) = build_menu(items, size) else { return 0 };
        // Without the window in front, the menu won't close on a click elsewhere
        let _ = SetForegroundWindow(hwnd);
        // A menu underlines its shortcut letters only after keyboard input, and the window's UI
        // state doesn't count, so a Shift tap it is. Lands on our window, which is in front.
        tap_shift();
        let picked = TrackPopupMenu(m, TPM_RETURNCMD, at.x, at.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        picked.0 as usize
    }
}

/// A popup menu of `items`, submenus and all. Destroying it destroys them too.
///
/// # Safety
/// The caller destroys what it returns.
unsafe fn build_menu(items: &[Item], size: u32) -> Option<HMENU> {
    // SAFETY: as the caller promises; a submenu that fails goes with its parent
    unsafe {
        let m = CreatePopupMenu().ok()?;
        for (n, i) in items.iter().enumerate() {
            let text = wide(&i.text);
            let (flags, id) = match i.sub.is_empty() {
                true => (i.flags, i.id),
                false => match build_menu(&i.sub, size) {
                    Some(sub) => (i.flags | MF_POPUP, sub.0 as usize),
                    None => (i.flags | MF_GRAYED, 0),
                },
            };
            let _ = AppendMenuW(m, flags, id, PCWSTR(text.as_ptr()));
            if let Some(b) = i.signal.and_then(|s| signal_bitmap(s, size)) {
                let info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_BITMAP,
                    hbmpItem: b,
                    ..Default::default()
                };
                let _ = SetMenuItemInfoW(m, n as u32, true, &info);
            }
        }
        Some(m)
    }
}

/// Shift down and up, as keyboard input
fn tap_shift() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_SHIFT, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // SAFETY: two well-formed keyboard inputs
    let _ = unsafe { SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32) };
}

/// Copies `s` into a fixed UTF-16 field, truncated, NUL-terminated
fn put(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    let mut i = 0;
    for c in s.encode_utf16().take(n) {
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// `s` as menu text, where a lone & would underline the next letter. For anything we didn't
/// write: a controller's name starts out as whatever it said over WPS.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

/// What to call a controller: its name, or a stand-in without one
fn name_of(name: Option<&str>) -> String {
    name.map_or_else(|| tr!("controller-unnamed"), Into::into)
}

/// The tooltip with something connected: each controller's menu line without the port or the
/// shortcut. By port, then the order they connected in, so the newest is at the bottom; without a
/// port, after those with one. `watch` is a controller's percent shown and its connection.
/// Windows cuts the tooltip at 127 characters: four 15-character names charging at 99% come to
/// exactly that.
fn tip_lines(controllers: &[Connected], watch: impl Fn(&[u8; 6]) -> (Option<u8>, Option<u64>)) -> String {
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
fn charge_state(b: &Battery, pct: u8) -> &'static str {
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

fn shunned_line(s: &Shunned) -> String {
    tr!("controller-shunned", name = menu_text(&name_of(s.name.as_deref())))
}

/// The controller the icon follows: of those with a battery reading, one running down before
/// one on the charger, charged or not, then the lowest percent shown
fn lowest(controllers: &[Connected], pct: impl Fn(&[u8; 6]) -> Option<u8>) -> Option<&Connected> {
    controllers
        .iter()
        .filter_map(|c| Some((c, c.battery?)))
        .min_by_key(|(c, b)| (b.charging, pct(&c.mac).unwrap_or(u8::MAX)))
        .map(|(c, _)| c)
}

/// The menu for `s`, with each controller's percent as the tray shows it
fn menu_items(s: &Status, pct: impl Fn(&[u8; 6]) -> Option<u8>) -> Vec<Item> {
    let gray = |on: bool| if on { MF_STRING } else { MF_STRING | MF_GRAYED };
    let title = MF_STRING | MF_DISABLED;
    let mut items = vec![
        Item::new(MF_STRING, id::ABOUT, tr!("menu-title", version = about_info().version)),
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
            Item::new(MF_STRING, id::of(n, id::IDENTIFY), tr!("controller-identify")),
            Item::new(MF_STRING, id::of(n, id::RENAME), tr!("controller-rename")),
            Item::new(
                MF_STRING,
                id::of(n, id::VBAN),
                if c.vban { tr!("controller-audio-off") } else { tr!("controller-audio-on") },
            ),
            Item::new(MF_STRING, id::of(n, id::DISCONNECT), tr!("controller-disconnect")),
        ]);
        let line =
            Item { sub, signal: c.signal, ..Item::new(MF_STRING, 0, controller_line(c, pct(&c.mac), &mut used)) };
        items.push(line);
    }
    for sh in &s.shunned {
        items.push(Item::new(gray(false), 0, shunned_line(sh)));
    }
    if s.controllers.is_empty() && s.shunned.is_empty() {
        items.push(Item::new(gray(false), 0, tr!("menu-none-connected")));
    }
    items.extend([
        Item::separator(),
        // Already open, or no room for another
        Item::new(gray(!s.pairing && s.controllers.len() < MAX_CONTROLLERS), id::PAIR, pair),
        Item::new(MF_STRING, id::RECONFIGURE, reconfigure),
        Item::separator(),
        Item::new(MF_STRING, id::EXIT, exit),
    ]);
    items
}

impl State {
    fn load_icons(&mut self) -> Result<()> {
        self.free_icons();
        let palette = if self.light { &LIGHT } else { &DARK };
        // SAFETY: plain query
        let size = small_icon_size(unsafe { GetDpiForSystem() });
        for icon in Icon::all() {
            let badges = BADGES.map(Some).filter(|_| icon.takes_badge());
            for badge in std::iter::once(None).chain(badges) {
                self.icons.push((icon, badge, render(&icon.badged(palette, size, badge)?)?));
            }
        }
        debug!(size, light = self.light, "tray icons rendered");
        Ok(())
    }

    fn free_icons(&mut self) {
        for (_, _, h) in self.icons.drain(..) {
            // SAFETY: created by load_icons, no longer referenced
            let _ = unsafe { DestroyIcon(h) };
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: self.hwnd, uID: 1, ..Default::default() }
    }

    /// Icon, tooltip and battery notifications for the current status
    fn refresh(&mut self) {
        let now = Instant::now();
        let msg = if self.shown.is_some() { NIM_MODIFY } else { NIM_ADD };
        let s = self.status.clone();
        // Not for pairing that timed out, a join that gave up, or a controller we sent away to
        // sleep, which would otherwise flash every time a flat one came back
        let disconnected = !s.pairing && !s.joining && s.controllers.is_empty();
        self.had_controller |= !s.controllers.is_empty();
        let left = self.had_controller && s.shunned.is_empty();
        if disconnected && !self.was_disconnected && (self.first || left) {
            self.flash_until = Some(now + FLASH_FOR);
        }
        if disconnected {
            self.had_controller = false;
        }
        self.was_disconnected = disconnected;
        self.first = false;

        // Each controller's gauge and alerts. What the charger did belongs to a connection.
        let reading = |m: &[u8; 6]| s.controllers.iter().find(|c| c.mac == *m).and_then(|c| c.battery);
        for (mac, w) in &mut self.watches {
            if reading(mac).is_none() {
                w.gauge.forget_charger();
                w.was_charging = false;
                w.was_charged = false;
            }
        }
        for (mac, w) in &mut self.watches {
            if !s.controllers.iter().any(|c| c.mac == *mac) {
                w.connection = None;
            }
        }
        for c in &s.controllers {
            let w = self.watches.entry(c.mac).or_default();
            if w.connection.is_none() {
                w.connection = Some(self.connections);
                self.connections += 1;
            }
        }
        let mut alerts = Vec::new();
        for c in &s.controllers {
            let Some(b) = c.battery else { continue };
            let w = self.watches.entry(c.mac).or_default();
            let (shown, fresh) = w.gauge.feed(&b, now);
            w.pct = Some(shown);
            if fresh || b.charging {
                w.critical_alerted = false;
                w.low_alerted = false;
            }
            let tell_must_charge =
                b.must_charge && w.must_charge_told.is_none_or(|t| now.duration_since(t) >= MUST_CHARGE_EVERY);
            if b.must_charge {
                // Says it all already
                w.critical_alerted = true;
            }
            if tell_must_charge {
                w.must_charge_told = Some(now);
            }
            // Once per discharge, when rumble cuts out
            let alert = !b.charging && b.critical && !w.critical_alerted;
            if alert {
                w.critical_alerted = true;
            }
            // Once per discharge, at the controller's 25% band; critical says it instead if it
            // gets there first
            let low = !b.charging && b.band < 50 && !w.low_alerted && !w.critical_alerted;
            if !b.charging && (b.band < 50 || w.critical_alerted) {
                w.low_alerted = true;
            }
            let name = name_of(c.name.as_deref());
            let charged = charge_state(&b, shown) == "charged";
            // The charging bit coming on. The gauge only reads full once the charger lets go, so
            // this is nearly always the charging notice, then the charged one up to 30 s later.
            if b.charging && !w.was_charging {
                w.charge_flash_until = Some(now + FLASH_FOR);
                // Already showing full: just that it's charged
                if !charged {
                    let notice = (tr!("alert-charging-title", name = &name), tr!("alert-charging", pct = shown));
                    alerts.push((Notice::Info, notice.0, notice.1));
                }
            }
            if charged && !w.was_charged {
                alerts.push((Notice::Info, tr!("alert-charged-title", name = &name), tr!("alert-charged")));
            }
            w.was_charging = b.charging;
            w.was_charged = charged;
            if low {
                alerts.push((Notice::Warning, tr!("alert-low-title", name = &name), tr!("alert-low", pct = shown)));
            }
            if alert {
                let notice = (tr!("alert-critical-title", name = &name), tr!("alert-critical", pct = shown));
                alerts.push((Notice::Alarm, notice.0, notice.1));
            }
            if tell_must_charge {
                alerts.push((Notice::Alarm, tr!("alert-empty-title", name = &name), tr!("alert-empty")));
            }
        }

        let n = s.controllers.len();
        self.badge = u8::try_from(n).ok().filter(|n| BADGES.contains(n));
        let pct = |m: &[u8; 6]| self.watches.get(m).and_then(|w| w.pct);
        let up = lowest(&s.controllers, pct).cloned();
        self.focus = up.as_ref().map(|c| c.mac);
        if s.pairing {
            let icon = self.animate(Icon::Pairing(0), now);
            self.show(icon, msg, Some(&tr!("tip-pairing")));
        } else if s.joining && up.is_none() {
            let icon = self.animate(Icon::Spinner(0), now);
            self.show(icon, msg, Some(&tr!("tip-joining")));
        } else if let Some(c) = up {
            let b = c.battery.expect("lowest has a reading");
            let shown = self.watches.get(&c.mac).and_then(|w| w.pct).unwrap_or(0);
            // Plugged in: the bolt flashes for a bit, already charged or not, then the icon settles
            let settled = Icon::of(&b, shown);
            self.settled = Some(settled);
            let icon = match settled {
                Icon::Charging | Icon::Charged if self.charge_flashing(now) => self.animate(Icon::Charging, now),
                _ => settled,
            };
            let tip = self.tip_lines(&s.controllers);
            self.show(icon, msg, Some(&tip));
        } else {
            let tip = match s.controllers.is_empty() {
                false => self.tip_lines(&s.controllers),
                true => tr!("tip-disconnected"),
            };
            // Flashing only with nothing connected, and only for a bit
            let icon = if self.flashing(now) { self.animate(Icon::Disconnected, now) } else { Icon::Disconnected };
            self.show(icon, msg, Some(&tip));
        }
        for (kind, title, text) in alerts {
            self.notify(&title, &text, kind);
        }
    }

    fn tip_lines(&self, controllers: &[Connected]) -> String {
        tip_lines(controllers, |m| self.watches.get(m).map_or((None, None), |w| (w.pct, w.connection)))
    }

    /// The animation's next frame, if what's on screen is animated
    fn next_frame(&self) -> Option<Icon> {
        match self.shown {
            // Once the flash is over, back to the plain frame and stay there
            Some(Icon::Disconnected) if !self.flashing(Instant::now()) => None,
            Some(Icon::DisconnectedRed) if !self.flashing(Instant::now()) => Some(Icon::Disconnected),
            // Likewise the bolt, settling on Charging, or Charged if it's full
            Some(i @ (Icon::Charging | Icon::ChargingLit)) if !self.charge_flashing(Instant::now()) => {
                self.settled.filter(|&s| s != i)
            }
            shown => shown.and_then(Icon::next_frame),
        }
    }

    /// Until the frame on screen is due to change. A flash's last frame ends with the flash.
    fn frame_left(&self) -> Duration {
        let Some(i) = self.shown else { return IDLE_TICK };
        let now = Instant::now();
        let charge_over = matches!(i, Icon::Charging | Icon::ChargingLit) && !self.charge_flashing(now);
        if (i == Icon::DisconnectedRed && !self.flashing(now)) || charge_over {
            return Duration::ZERO;
        }
        i.frame_time().saturating_sub(self.frame_at.elapsed())
    }

    /// The disconnected icon should be flashing
    fn flashing(&self, now: Instant) -> bool {
        self.was_disconnected && self.flash_until.is_some_and(|t| now < t)
    }

    /// The charging bolt should be flashing, on the controller the icon shows
    fn charge_flashing(&self, now: Instant) -> bool {
        let w = self.focus.and_then(|m| self.watches.get(&m));
        w.is_some_and(|w| w.was_charging && w.charge_flash_until.is_some_and(|t| now < t))
    }

    /// The frame of `icon` to show; a new animation starts its clock. The run loop steps it
    /// from there.
    fn animate(&mut self, icon: Icon, now: Instant) -> Icon {
        let kept = Icon::keep_frame(icon, self.shown);
        if kept == icon && self.shown != Some(icon) {
            self.frame_at = now;
        }
        kept
    }

    fn show(&mut self, icon: Icon, msg: NOTIFY_ICON_MESSAGE, tip: Option<&str>) {
        let badge = self.badge.filter(|_| icon.takes_badge());
        let Some(&(.., h)) = self.icons.iter().find(|(i, b, _)| (*i, *b) == (icon, badge)) else { return };
        let mut d = self.data();
        d.uFlags = NIF_ICON | NIF_SHOWTIP | NIF_MESSAGE;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = h;
        if let Some(tip) = tip {
            d.uFlags |= NIF_TIP;
            put(&mut d.szTip, tip);
        }
        // SAFETY: d is a filled-in NOTIFYICONDATAW for our window
        if unsafe { Shell_NotifyIconW(msg, &d) }.as_bool() {
            self.shown = Some(icon);
        } else {
            debug!("Shell_NotifyIcon failed");
        }
    }

    fn hide(&mut self) {
        if self.shown.take().is_some() {
            let d = self.data();
            // SAFETY: as in show
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &d) };
        }
    }

    /// `alarm`: Windows' critical battery sound and the error icon, else a plain warning
    fn notify(&self, title: &str, text: &str, kind: Notice) {
        info!(title, text, ?kind, "notification");
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        put(&mut d.szInfoTitle, title);
        put(&mut d.szInfo, text);
        d.dwInfoFlags = match kind {
            Notice::Info => NIIF_INFO,
            Notice::Warning => NIIF_WARNING,
            // Our own sound instead of the generic one
            Notice::Alarm => NIIF_ERROR | NIIF_NOSOUND,
        };
        // SAFETY: as in show; PlaySound copies nothing from us after returning
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
            if matches!(kind, Notice::Alarm) {
                let _ = PlaySoundW(w!("CriticalBatteryAlarm"), None, SND_ALIAS | SND_ASYNC);
            }
        }
    }

    /// The menu for the current status
    fn menu_items(&self) -> Vec<Item> {
        menu_items(&self.status, |m| self.watches.get(m).and_then(|w| w.pct))
    }

    /// Takes in what Tray sent, follows the taskbar theme, steps the animation, flags a stale
    /// menu, and sets the timer for the next go
    fn tick(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Status(s)) => {
                    self.status = s;
                    self.refresh();
                }
                Ok(Msg::Problem(title, text)) => self.notify(&title, &text, Notice::Warning),
                Ok(Msg::Quit) | Err(TryRecvError::Disconnected) => return quit(),
                Err(TryRecvError::Empty) => break,
            }
        }
        if READD_WANTED.replace(false) {
            self.shown = None;
            self.refresh();
        } else {
            let now = Instant::now();
            // Every gauge gets its look, not just the first that's due
            if self.watches.values_mut().map(|w| w.gauge.look_due(now)).fold(false, |a, due| a | due) {
                self.refresh();
            }
        }
        if self.last_theme.elapsed() >= THEME_EVERY {
            self.last_theme = Instant::now();
            let light = light_taskbar();
            if light != self.light {
                self.light = light;
                match self.load_icons() {
                    Ok(()) => {
                        if let Some(i) = self.shown {
                            self.show(i, NIM_MODIFY, None);
                        }
                    }
                    Err(e) => warn!("tray icons: {e:#}"),
                }
            }
        }
        if let Some(next) = self.next_frame()
            && self.frame_left().is_zero()
        {
            self.frame_at = Instant::now();
            self.show(next, NIM_MODIFY, None);
        }
        // Not for the signal bars alone: reopening would collapse an open submenu each time a link
        // flickers. They catch up the next time the menu opens.
        let stale = MENU_SHOWN
            .with_borrow(|shown| shown.as_ref().is_some_and(|items| *items != without_signal(&self.menu_items())));
        if stale {
            MENU_REOPEN.set(true);
            // SAFETY: plain call; closes the menu this thread has open
            let _ = unsafe { EndMenu() };
        }
        // Up to the next animation frame, at most IDLE_TICK
        let wait = if self.next_frame().is_some() { self.frame_left().min(IDLE_TICK) } else { IDLE_TICK };
        // SAFETY: our window, on its thread; a new SetTimer with the same ID replaces the old
        unsafe { SetTimer(Some(self.hwnd), TICK_TIMER, (wait.as_millis() as u32).max(1), None) };
    }
}

/// The rename dialog's edit box
const RENAME_EDIT: i32 = 100;

/// Asks for a new name for `c`. Returns it cleaned up, or None if cancelled or left blank.
fn rename(hwnd: HWND, c: &Connected) -> Option<String> {
    RENAME_TEXT.set(wide(c.name.as_deref().unwrap_or("")));
    let prompt = tr!("rename-prompt", mac = c.mac.fmt(), max = MAX_NAME);
    let template = rename_template(&prompt);
    // SAFETY: a complete template, DWORD-aligned, that outlives the call; rename_proc is 'static
    let r = unsafe {
        let instance = GetModuleHandleW(None).ok().map(Into::into);
        DialogBoxIndirectParamW(instance, template.as_ptr().cast(), Some(hwnd), Some(rename_proc), LPARAM(0))
    };
    if r != IDOK.0 as isize {
        return None;
    }
    let text = RENAME_TEXT.take();
    shdlib::names::clean(&String::from_utf16_lossy(&text))
}

extern "system" fn rename_proc(dlg: HWND, msg: u32, wp: WPARAM, _: LPARAM) -> isize {
    // SAFETY: dlg is the dialog, on this thread; the buffers outlive each call
    unsafe {
        if msg == WM_INITDIALOG {
            let text = RENAME_TEXT.with_borrow(Clone::clone);
            let _ = SetDlgItemTextW(dlg, RENAME_EDIT, PCWSTR(text.as_ptr()));
            SendDlgItemMessageW(dlg, RENAME_EDIT, EM_LIMITTEXT, WPARAM(MAX_NAME), LPARAM(0));
            let _ = SetForegroundWindow(dlg);
            // TRUE: focus to the first tab stop, the edit box, with its text selected
            return 1;
        }
        if msg == WM_COMMAND {
            let id = (wp.0 & 0xFFFF) as i32;
            if id == IDOK.0 {
                let mut buf = [0u16; 64];
                let n = GetDlgItemTextW(dlg, RENAME_EDIT, &mut buf) as usize;
                RENAME_TEXT.set(buf[..n].to_vec());
                let _ = EndDialog(dlg, IDOK.0 as isize);
                return 1;
            }
            if id == IDCANCEL.0 {
                let _ = EndDialog(dlg, IDCANCEL.0 as isize);
                return 1;
            }
        }
        0
    }
}

/// The rename dialog, in dialog units: a prompt, the edit box, then OK and Cancel. Words, as
/// DialogBoxIndirect reads them, packed into a DWORD-aligned buffer.
fn rename_template(prompt: &str) -> Vec<u32> {
    fn dword(w: &mut Vec<u16>, v: u32) {
        w.extend([v as u16, (v >> 16) as u16]);
    }
    fn text(w: &mut Vec<u16>, s: &str) {
        w.extend(s.encode_utf16().chain([0]));
    }
    fn item(w: &mut Vec<u16>, style: u32, rect: [i16; 4], id: u16, class: u16, title: &str) {
        // Each item starts on a DWORD
        if w.len() % 2 == 1 {
            w.push(0);
        }
        dword(w, (WS_CHILD | WS_VISIBLE).0 | style);
        dword(w, 0);
        w.extend(rect.map(|v| v as u16));
        w.push(id);
        // A predefined class, by atom
        w.extend([0xFFFF, class]);
        text(w, title);
        // No creation data
        w.push(0);
    }
    const BUTTON: u16 = 0x80;
    const EDIT: u16 = 0x81;
    const STATIC: u16 = 0x82;
    let mut w = Vec::new();
    dword(&mut w, (WS_POPUP | WS_CAPTION | WS_SYSMENU).0 | (DS_MODALFRAME | DS_SETFONT | DS_CENTER) as u32);
    dword(&mut w, 0);
    // Items, then x, y, width, height
    w.extend([4, 0, 0, 220, 62]);
    // No menu, the default class
    w.extend([0, 0]);
    text(&mut w, &tr!("rename-title"));
    w.push(9);
    text(&mut w, "Segoe UI");
    item(&mut w, 0, [7, 7, 206, 9], 0xFFFF, STATIC, prompt);
    let edit = (WS_BORDER | WS_TABSTOP).0 | ES_AUTOHSCROLL as u32;
    item(&mut w, edit, [7, 19, 206, 14], RENAME_EDIT as u16, EDIT, "");
    item(&mut w, WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32, [109, 41, 50, 14], IDOK.0 as u16, BUTTON, &tr!("dialog-ok"));
    item(
        &mut w,
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        [163, 41, 50, 14],
        IDCANCEL.0 as u16,
        BUTTON,
        &tr!("dialog-cancel"),
    );
    if w.len() % 2 == 1 {
        w.push(0);
    }
    w.chunks(2).map(|c| u32::from(c[0]) | u32::from(c[1]) << 16).collect()
}

/// Version, build, author, and the notices the GPL and the bundled works' licenses ask for
fn about(hwnd: HWND) {
    let info = about_info();
    let author = info.authors.replace(':', ", ");
    let text = tr!("about", version = info.version, built = info.built, author = author);
    message(hwnd, &text, &tr!("about-title"), MB_OK | MB_ICONINFORMATION);
}

/// How to pair, before turning it on
fn confirm_pair(hwnd: HWND) -> bool {
    message(hwnd, &tr!("pair"), &tr!("pair-title"), MB_OKCANCEL | MB_ICONINFORMATION) == IDOK
}

/// A new network throws every pairing away, so ask first
fn confirm_reconfigure(hwnd: HWND) -> bool {
    let style = MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2;
    message(hwnd, &tr!("reconfigure"), &tr!("reconfigure-title"), style) == IDOK
}

/// A message box in front, until it's closed
fn message(hwnd: HWND, text: &str, title: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let (text, title) = (wide(text), wide(title));
    // SAFETY: text and title outlive the call
    unsafe { MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style | MB_SETFOREGROUND) }
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;
    use resvg::tiny_skia::PixmapPaint;

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

    /// Every icon in both themes, on its taskbar color, for eyeballing: one PNG each, and strips
    /// of the status icons, the pairing animation and the spinner. Each strip's first row is
    /// plain, then a row for each count badge; the icons that take none leave those rows empty.
    #[test]
    fn renders_icons_to_target() {
        const SIZE: u32 = 64;
        const GAP: u32 = 8;
        let badges: Vec<Option<u8>> = std::iter::once(None).chain(BADGES.map(Some)).collect();
        let rows = badges.len() as u32;
        let dir = icon_dir();
        for (theme, p, bg) in [("light", &LIGHT, [0xEE, 0xEE, 0xEE]), ("dark", &DARK, [0x20, 0x20, 0x20])] {
            let out = dir.join(theme);
            std::fs::create_dir_all(&out).unwrap();
            let bg = resvg::tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], 0xFF);
            let strip_of = |n: u32| {
                let mut s = Pixmap::new(n * SIZE + (n - 1) * GAP, rows * SIZE + (rows - 1) * GAP).unwrap();
                s.fill(resvg::tiny_skia::Color::from_rgba8(0x80, 0x80, 0x80, 0xFF));
                s
            };
            let on_bg = |icon: Icon, badge: Option<u8>| {
                let mut pm = Pixmap::new(SIZE, SIZE).unwrap();
                pm.fill(bg);
                let fg = icon.badged(p, SIZE, badge).unwrap();
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
                    let (x, y) = ((i * (SIZE + GAP)) as i32, (row as u32 * (SIZE + GAP)) as i32);
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
            // The menu's always light, so its signal bars are the same either way
            if theme == "light" {
                let mut signal_strip = Pixmap::new(4 * SIZE + 3 * GAP, SIZE).unwrap();
                signal_strip.fill(resvg::tiny_skia::Color::from_rgba8(0x80, 0x80, 0x80, 0xFF));
                let menu_bg = resvg::tiny_skia::Color::from_rgba8(0xF9, 0xF9, 0xF9, 0xFF);
                for (i, s) in SIGNALS.into_iter().enumerate() {
                    let mut pm = Pixmap::new(SIZE, SIZE).unwrap();
                    pm.fill(menu_bg);
                    let fg = rasterize(&signal_svg(s), SIZE).unwrap();
                    pm.draw_pixmap(0, 0, fg.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                    pm.save_png(out.join(format!("signal_{}.png", format!("{s:?}").to_lowercase()))).unwrap();
                    let x = (i as u32 * (SIZE + GAP)) as i32;
                    signal_strip.draw_pixmap(x, 0, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
                signal_strip.save_png(out.join("signal_strip.png")).unwrap();
            }
            strip.save_png(out.join("pairing_strip.png")).unwrap();
            spinner_strip.save_png(out.join("spinner_strip.png")).unwrap();
        }
    }

    const SIGNALS: [Signal; 4] = [Signal::Dropping, Signal::Low, Signal::Fair, Signal::Good];

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
    fn menu_items_carry_their_bars() {
        use windows::Win32::UI::WindowsAndMessaging::GetMenuItemInfoW;
        let s = Status { controllers: vec![up(1, "A", false)], ..Default::default() };
        let items = menu_items(&s, |_| Some(50));
        let at = items.iter().position(|i| i.signal.is_some()).unwrap();
        // SAFETY: our menu, destroyed below; info outlives the call
        unsafe {
            let m = build_menu(&items, 16).unwrap();
            let mut info =
                MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_BITMAP, ..Default::default() };
            GetMenuItemInfoW(m, at as u32, true, &mut info).unwrap();
            // The cached one, rendered once
            assert_eq!(Some(info.hbmpItem), signal_bitmap(Signal::Good, 16));
            assert_eq!(SIGNAL_BITMAPS.with_borrow(Vec::len), 1);
            let _ = DestroyMenu(m);
        }
        // A change in the bars alone doesn't make an open menu stale
        let mut weaker = s.clone();
        weaker.controllers[0].signal = Some(Signal::Low);
        let weaker = menu_items(&weaker, |_| Some(50));
        assert_ne!(weaker, items);
        assert_eq!(without_signal(&weaker), without_signal(&items));
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
            let b = menu_bitmap(&pm).unwrap();
            // SAFETY: just made, used by nothing
            let _ = unsafe { DeleteObject(b.into()) };
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
        let pm = Icon::Full.badged(&DARK, 32, Some(3)).unwrap();
        // The disc is in the corner, where the plain icon has none of its blue
        let blue = |pm: &Pixmap| pm.pixel(28, 28).unwrap().demultiply().blue();
        assert!(blue(&pm) > 0xC0 && blue(&Icon::Full.pixmap(&DARK, 32).unwrap()) < 0x40);
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

    fn up(mac: u8, name: &str, vban: bool) -> Connected {
        let battery = Battery { charging: false, band: 50, reading: 0xD500, critical: false, must_charge: false };
        Connected {
            mac: [mac; 6],
            name: Some(name.into()),
            port: Some(mac),
            battery: Some(battery),
            vban,
            signal: Some(Signal::Good),
            firmware: Some("1.96/99/3.71/0.32".into()),
        }
    }

    #[test]
    fn lines() {
        let sh = |name: Option<&str>| Shunned { mac: [0; 6], name: name.map(Into::into) };
        assert_eq!(shunned_line(&sh(Some("nVidiaBlake"))), "nVidiaBlake (shunned)");
        assert_eq!(shunned_line(&sh(None)), "Controller (shunned)");
        assert_eq!(shunned_line(&sh(Some("Pad&Co"))), "Pad&&Co (shunned)");
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
            shunned: vec![Shunned { mac: [3; 6], name: None }],
            ..Default::default()
        };
        let items = menu_items(&s, |m| Some(m[0] * 10));
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        // The bars go on each controller's line and nowhere else
        let signals: Vec<_> = items.iter().filter_map(|i| i.signal).collect();
        assert_eq!(signals, [Signal::Good, Signal::Good]);
        assert_eq!(items[3].signal, Some(Signal::Good));
        assert_eq!(
            texts,
            [
                &format!("shieldd (v{})", about_info().version),
                " > 2 connected.",
                "",
                "&A: Port #1 (10%)",
                "&B: Port #2 (20%)",
                "Controller (shunned)",
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
                (id::of(1, id::VBAN), "Disable &audio"),
                (id::of(1, id::DISCONNECT), "&Disconnect"),
            ]
        );
        assert!(!items[3].sub.iter().any(|i| i.text.starts_with("Firmware")));
        assert_eq!(id::controller(id::of(1, id::VBAN)), Some((1, id::VBAN)));
        assert_eq!(id::controller(id::EXIT), None);
        assert!(items[5].flags.0 & MF_GRAYED.0 != 0 && items[5].sub.is_empty());
        // Pairing grays its line; with nothing at all there's a line to say so
        let none = menu_items(&Status { pairing: true, ..Default::default() }, |_| None);
        let at = |t: &str| none.iter().find(|i| i.text == t).unwrap().flags.0 & MF_GRAYED.0 != 0;
        assert!(at("No controllers connected") && at("&Pair controller..."));
        // Four connected: no room to pair a fifth
        let pair_grayed = |n: u8| {
            let s = Status { controllers: (1..=n).map(|m| up(m, "A", false)).collect(), ..Default::default() };
            let items = menu_items(&s, |_| Some(50));
            items.iter().find(|i| i.id == id::PAIR).unwrap().flags.0 & MF_GRAYED.0 != 0
        };
        assert!(!pair_grayed(3));
        assert!(pair_grayed(4));
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

    #[test]
    fn the_icon_follows_the_lowest() {
        let cs = [up(1, "A", false), up(2, "B", false), Connected { mac: [3; 6], ..Default::default() }];
        let pct = |m: &[u8; 6]| [None, Some(60), Some(30), None][usize::from(m[0])];
        assert_eq!(lowest(&cs, pct).map(|c| c.mac), Some([2; 6]));
        assert_eq!(lowest(&cs[2..], pct).map(|c| c.mac), None, "no reading, nothing to show");
        // On the charger, lower or charged, it still loses to one running down
        let mut charging = cs.clone();
        charging[1].battery.as_mut().unwrap().charging = true;
        assert_eq!(lowest(&charging, pct).map(|c| c.mac), Some([1; 6]));
        let full = |m: &[u8; 6]| [None, Some(100), Some(100), None][usize::from(m[0])];
        charging.swap(0, 1);
        assert_eq!(lowest(&charging, full).map(|c| c.mac), Some([1; 6]));
        charging[1].battery.as_mut().unwrap().charging = true;
        assert_eq!(lowest(&charging, pct).map(|c| c.mac), Some([2; 6]), "all charging, the lowest");
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
