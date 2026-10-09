//! The Windows tray: an icon for the controller's battery (mailbox 01, shieldd's controller.rs),
//! with a notification when it goes critical and another when it's kicked for being too flat to
//! use. Shown as long as shieldd runs, disconnected or not.
//!
//! Battery icons are Ikono Bold Line Icons, turned upright; the disconnected and pairing ones
//! are from Clarity. All from shieldtray/res/, MIT, see THIRD_PARTY.md. Rendered at the tray's
//! size and colored for the taskbar's light or dark theme.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT, SND_SYSTEM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::EM_LIMITTEXT;
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, TOOLTIPS_CLASSW, TTF_ABSOLUTE, TTF_TRACK,
    TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH, TTM_TRACKACTIVATE, TTM_TRACKPOSITION, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP,
    TTS_NOPREFIX, TTTOOLINFOW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetSystemMetricsForDpi, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_NOSOUND, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CW_USEDEFAULT, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DS_CENTER, DS_MODALFRAME, DS_SETFONT, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DialogBoxIndirectParamW, DispatchMessageW, ES_AUTOHSCROLL, EndDialog, EndMenu, EnumThreadWindows, GetClassNameW,
    GetCursorPos, GetDlgItemTextW, GetMenuItemID, GetMenuState, GetMessageW, HICON, HMENU, HWND_TOPMOST, ICONINFO,
    IDCANCEL, IDOK, KillTimer, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND,
    MENU_ITEM_FLAGS, MF_BYPOSITION, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SendDlgItemMessageW, SendMessageW, SetDlgItemTextW, SetForegroundWindow, SetTimer, SetWindowPos, TPM_RETURNCMD,
    TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_ENTERIDLE, WM_INITDIALOG,
    WM_LBUTTONUP, WM_MENURBUTTONUP, WM_MENUSELECT, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::battery::{Battery, Gauge};
use crate::{About, Command, Connected, Shunned, Status};
use shieldutl::names::MAX_NAME;
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
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).context("parsing icon")?;
    let mut pm = Pixmap::new(size, size).context("icon pixmap")?;
    let bb = tree.root().abs_stroke_bounding_box();
    let side = size as f32;
    let scale = side / bb.width().max(bb.height());
    let tx = (side - bb.width() * scale) / 2.0 - bb.x() * scale;
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
    // SAFETY: the DIB is size x size x 4 bytes, exactly bgra's length; both bitmaps are
    // deleted once the icon has its own copies
    unsafe {
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).context("icon bitmap")?;
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        // All-zero AND mask: the alpha channel does the masking
        let mask = CreateBitmap(side, side, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.context("creating icon")
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

/// A menu item: flags, ID, text
type Item = (MENU_ITEM_FLAGS, usize, String);
/// The tray window, for Tray::drop; one per process
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// Menu item IDs; 0 is TrackPopupMenu's "nothing picked"
mod id {
    pub const PAIR: usize = 1;
    pub const VBAN: usize = 2;
    /// The controller line: identifies the controller, and WM_MENUSELECT knows it by this
    pub const CONTROLLER: usize = 3;
    pub const DISCONNECT: usize = 4;
    pub const EXIT: usize = 5;
    /// The title line
    pub const ABOUT: usize = 6;
    /// Not an item: the controller line, right-clicked
    pub const RENAME: usize = 7;
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
    /// Tooltip for the controller line, and its text while the menu is up
    static TIP: Cell<Option<HWND>> = const { Cell::new(None) };
    static TIP_TEXT: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static TIP_ON: Cell<bool> = const { Cell::new(false) };
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
    /// What a right-click in the open menu picked
    static RIGHT_PICK: Cell<Option<usize>> = const { Cell::new(None) };
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
    } else if msg == WM_MENURBUTTONUP {
        right_click(HMENU(lp.0 as *mut _), wp.0 as i32);
    } else if msg == WM_MENUSELECT {
        // Item ID in the low word; the menu, or none once it closes
        show_tip(hwnd, wp.0 & 0xFFFF == id::CONTROLLER && lp.0 != 0);
    } else if msg == WM_ENTERIDLE {
        // The menu loop has caught up, e.g. after a mouse move
        follow_mouse();
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

/// A right-click on item `pos` of the open menu: renames on the controller line, and picks
/// anything else as a left-click would
fn right_click(m: HMENU, pos: i32) {
    // SAFETY: the open menu, from WM_MENURBUTTONUP
    let (id, state) = unsafe { (GetMenuItemID(m, pos) as usize, GetMenuState(m, pos as u32, MF_BYPOSITION)) };
    if state & (MF_GRAYED.0 | MF_DISABLED.0 | MF_SEPARATOR.0) != 0 || id == 0 {
        return;
    }
    RIGHT_PICK.set(Some(if id == id::CONTROLLER { id::RENAME } else { id }));
    // SAFETY: plain call; closes the menu this thread has open
    let _ = unsafe { EndMenu() };
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

struct State {
    hwnd: HWND,
    rx: Receiver<Msg>,
    light: bool,
    last_theme: Instant,
    icons: Vec<(Icon, HICON)>,
    /// What's on screen, if the icon is shown
    shown: Option<Icon>,
    status: Status,
    gauge: Gauge,
    /// The gauge's last answer, for the menu
    pct: Option<u8>,
    critical_alerted: bool,
    /// Last must-charge notification
    must_charge_told: Option<Instant>,
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
    /// Last refresh found the controller on the charger
    was_charging: bool,
    /// The charging bolt flashes until then
    charge_flash_until: Option<Instant>,
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
    TIP.set(create_tip(hwnd));
    let mut st = State {
        hwnd,
        rx,
        light: light_taskbar(),
        last_theme: Instant::now(),
        icons: Vec::new(),
        shown: None,
        status: Status::default(),
        gauge: Gauge::default(),
        pct: None,
        critical_alerted: false,
        must_charge_told: None,
        frame_at: Instant::now(),
        was_disconnected: false,
        had_controller: false,
        first: true,
        flash_until: None,
        was_charging: false,
        charge_flash_until: None,
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
        let Some((items, tip, s, commands)) = with_state(|st| {
            let (items, tip) = st.menu_items();
            (items, tip, st.status.clone(), st.commands.clone())
        }) else {
            return;
        };
        TIP_TEXT.set(tip.as_deref().map(wide).unwrap_or_default());
        MENU_REOPEN.set(false);
        RIGHT_PICK.set(None);
        MENU_SHOWN.set(Some(items.clone()));
        let picked = track(hwnd, &items, at);
        MENU_SHOWN.set(None);
        // Before a reopen, which would lose it
        let picked = RIGHT_PICK.take().filter(|_| picked == 0).unwrap_or(picked);
        if picked == 0 && MENU_REOPEN.replace(false) && !QUITTING.get() {
            continue;
        }
        MENU_REOPEN.set(false);
        if picked == 0 && cursor_on_icon(hwnd) {
            CLOSED_ON_ICON.set(Some(Instant::now()));
        }
        break (picked, s, commands);
    };
    show_tip(hwnd, false);
    let connected = s.controller.is_some();
    let cmd = match picked {
        _ if QUITTING.get() => return,
        id::PAIR if confirm_pair(hwnd, connected) => Command::Pair,
        id::VBAN => Command::Vban(!s.vban),
        id::CONTROLLER => Command::Identify,
        id::RENAME => {
            let Some(c) = &s.controller else { return };
            let Some(name) = rename(hwnd, c) else { return };
            Command::Rename(c.mac, name)
        }
        id::DISCONNECT => Command::Disconnect,
        id::EXIT => Command::Exit,
        id::ABOUT => return about(hwnd),
        _ => return,
    };
    debug!(?cmd, "tray menu");
    let vban_on = cmd == Command::Vban(true);
    let _ = commands.send(cmd);
    if vban_on && !QUITTING.get() {
        let text = format!(
            "Controller audio over VBAN, for Voicemeeter:\n\n\
             Mic: stream {}, sent to 127.0.0.1 port {}\n\
             Headset: stream {}, received on port {}\n\n\
             Point an incoming stream at the first and an outgoing one at the second, PCM 16-bit.",
            shieldutl::vban::MIC_STREAM,
            shieldutl::vban::MIC_PORT,
            shieldutl::vban::PAD_STREAM,
            shieldutl::vban::PAD_PORT,
        );
        let text = wide(&text);
        // SAFETY: text outlives the call
        unsafe {
            MessageBoxW(
                Some(hwnd),
                PCWSTR(text.as_ptr()),
                w!("shieldd: VBAN on"),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            )
        };
    }
}

/// Pops up `items` at `at` until something's picked or it's closed; 0 for nothing
fn track(hwnd: HWND, items: &[Item], at: POINT) -> usize {
    // SAFETY: the menu is ours and destroyed below; the strings outlive AppendMenuW, which
    // copies them
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return 0 };
        for (flags, id, text) in items {
            let text = wide(text);
            let _ = AppendMenuW(m, *flags, *id, PCWSTR(text.as_ptr()));
        }
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

/// Shift down and up, as keyboard input
fn tap_shift() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_SHIFT, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // SAFETY: two well-formed keyboard inputs
    let _ = unsafe { SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32) };
}

/// A tracking tooltip, owned by the tray window; positioned and shown by show_tip
fn create_tip(owner: HWND) -> Option<HWND> {
    // SAFETY: plain window creation; the tool info is copied by TTM_ADDTOOLW
    unsafe {
        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES };
        let _ = InitCommonControlsEx(&icc);
        let tip = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            None,
            WS_POPUP | WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(owner),
            None,
            None,
            None,
        )
        .ok()?;
        let ti = tool(owner, w!("").as_ptr());
        SendMessageW(tip, TTM_ADDTOOLW, None, Some(LPARAM(&raw const ti as isize)));
        // Any max width turns on line breaks
        SendMessageW(tip, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(400)));
        Some(tip)
    }
}

/// The one tool on the tooltip. Sized without lpReserved, which comctl32 v5 refuses; there's no
/// manifest asking for v6.
fn tool(owner: HWND, text: *const u16) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: (size_of::<TTTOOLINFOW>() - size_of::<*mut core::ffi::c_void>()) as u32,
        uFlags: TTF_TRACK | TTF_ABSOLUTE,
        hwnd: owner,
        uId: 1,
        lpszText: PWSTR(text.cast_mut()),
        ..Default::default()
    }
}

/// Shows the tooltip at the mouse, or hides it
fn show_tip(owner: HWND, on: bool) {
    let Some(tip) = TIP.get() else { return };
    let on = on && TIP_TEXT.with_borrow(|t| t.len() > 1);
    if on == TIP_ON.get() {
        return;
    }
    TIP_ON.set(on);
    TIP_TEXT.with_borrow(|text| {
        let ti = tool(owner, text.as_ptr());
        let ti = LPARAM(&raw const ti as isize);
        // SAFETY: ti and text outlive the calls, which copy what they need
        unsafe {
            if on {
                SendMessageW(tip, TTM_UPDATETIPTEXTW, None, Some(ti));
                follow_mouse();
            }
            SendMessageW(tip, TTM_TRACKACTIVATE, Some(WPARAM(on.into())), Some(ti));
            if on {
                raise(tip);
            }
        }
    });
}

/// Moves a shown tooltip to just below and right of the pointer, where a regular one goes
fn follow_mouse() {
    let Some(tip) = TIP.get().filter(|_| TIP_ON.get()) else { return };
    let mut at = POINT::default();
    // SAFETY: plain queries and a message with no pointers
    unsafe {
        if GetCursorPos(&mut at).is_err() {
            return;
        }
        let (x, y) = (at.x + 16, at.y + 20);
        let packed = (x as u16 as u32) | ((y as u16 as u32) << 16);
        SendMessageW(tip, TTM_TRACKPOSITION, None, Some(LPARAM(packed as isize)));
        raise(tip);
    }
}

/// Above the menu, which is topmost too and opened after the tooltip was made
fn raise(tip: HWND) {
    // SAFETY: our own window; no move, size or focus change
    let _ = unsafe { SetWindowPos(tip, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
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

fn menu_name(name: Option<&str>) -> String {
    menu_text(name.unwrap_or("Controller"))
}

/// The controller line of the menu
fn controller_line(c: Option<&Connected>, pct: Option<u8>, shunned: Option<&Shunned>) -> String {
    let Some(c) = c else {
        return match shunned {
            Some(s) => format!("{} (shunned)", menu_name(s.name.as_deref())),
            None => "No controllers connected".into(),
        };
    };
    let name = menu_name(c.name.as_deref());
    let port = c.port.map_or_else(|| "?".into(), |p| p.to_string());
    match (c.battery, pct) {
        (Some(b), Some(p)) if b.charging && p >= 100 => format!("{name}: Port #{port} ({p}%) (charged)"),
        (Some(b), Some(p)) if b.charging => format!("{name}: Port #{port} ({p}%) (charging)"),
        (Some(_), Some(p)) => format!("{name}: Port #{port} ({p}%)"),
        _ => format!("{name}: Port #{port}"),
    }
}

impl State {
    fn load_icons(&mut self) -> Result<()> {
        self.free_icons();
        let palette = if self.light { &LIGHT } else { &DARK };
        // SAFETY: plain queries
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16) as u32;
        for icon in Icon::all() {
            self.icons.push((icon, render(&icon.pixmap(palette, size)?)?));
        }
        debug!(size, light = self.light, "tray icons rendered");
        Ok(())
    }

    fn free_icons(&mut self) {
        for (_, h) in self.icons.drain(..) {
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
        let disconnected = !s.pairing && !s.joining && s.controller.is_none();
        self.had_controller |= s.controller.is_some();
        let left = self.had_controller && s.shunned.is_none();
        if disconnected && !self.was_disconnected && (self.first || left) {
            self.flash_until = Some(now + FLASH_FOR);
        }
        if disconnected {
            self.had_controller = false;
        }
        self.was_disconnected = disconnected;
        self.first = false;
        let charging =
            !s.pairing && !s.joining && s.controller.as_ref().and_then(|c| c.battery).is_some_and(|b| b.charging);
        if charging && !self.was_charging {
            self.charge_flash_until = Some(now + FLASH_FOR);
        }
        self.was_charging = charging;
        if s.pairing {
            self.pct = None;
            let icon = self.animate(Icon::Pairing(0), now);
            self.show(icon, msg, Some("SHIELD controller: pairing"));
            return;
        }
        if s.joining {
            self.pct = None;
            let icon = self.animate(Icon::Spinner(0), now);
            self.show(icon, msg, Some("SHIELD controller: connecting..."));
            return;
        }
        let Some(b) = s.controller.as_ref().and_then(|c| c.battery) else {
            let tip = match s.controller {
                Some(_) => "SHIELD controller: connected, no battery reading",
                None => "SHIELD controller: not connected",
            };
            self.pct = None;
            self.gauge.forget_charger();
            // Flashing only with nothing connected, and only for a bit
            let icon = if self.flashing(now) { self.animate(Icon::Disconnected, now) } else { Icon::Disconnected };
            self.show(icon, msg, Some(tip));
            return;
        };
        let (shown, fresh) = self.gauge.feed(&b, now);
        self.pct = Some(shown);
        if fresh || b.charging {
            self.critical_alerted = false;
        }
        let tell_must_charge =
            b.must_charge && self.must_charge_told.is_none_or(|t| now.duration_since(t) >= MUST_CHARGE_EVERY);
        if b.must_charge {
            // Says it all already
            self.critical_alerted = true;
        }
        if tell_must_charge {
            self.must_charge_told = Some(now);
        }
        // Once per discharge, when rumble cuts out
        let alert = !b.charging && b.critical && !self.critical_alerted;
        if alert {
            self.critical_alerted = true;
        }
        // Plugged in: the bolt flashes for a bit, already charged or not, then the icon settles
        let settled = Icon::of(&b, shown);
        self.settled = Some(settled);
        let icon = match settled {
            Icon::Charging | Icon::Charged if self.charge_flashing(now) => self.animate(Icon::Charging, now),
            _ => settled,
        };
        let tip = match settled {
            Icon::Charged => format!("SHIELD controller: {shown}% (charged)"),
            _ if b.charging => format!("SHIELD controller: {shown}% (charging)"),
            _ => format!("SHIELD controller: {shown}%"),
        };
        self.show(icon, msg, Some(&tip));
        if alert {
            self.notify("Controller battery critical", &format!("{shown}% left. Rumble is off; charge it now."), true);
        }
        if tell_must_charge {
            self.notify("Controller battery empty", "Disconnected; plug in the controller to use it.", true);
        }
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

    /// The charging bolt should be flashing
    fn charge_flashing(&self, now: Instant) -> bool {
        self.was_charging && self.charge_flash_until.is_some_and(|t| now < t)
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
        let Some(&(_, h)) = self.icons.iter().find(|(i, _)| *i == icon) else { return };
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
    fn notify(&self, title: &str, text: &str, alarm: bool) {
        info!(title, text, "notification");
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        put(&mut d.szInfoTitle, title);
        put(&mut d.szInfo, text);
        // Our own sound instead of the generic one
        d.dwInfoFlags = if alarm { NIIF_ERROR | NIIF_NOSOUND } else { NIIF_WARNING };
        // SAFETY: as in show; PlaySound copies nothing from us after returning
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
            if alarm {
                let _ = PlaySoundW(w!("CriticalBatteryAlarm"), None, SND_ALIAS | SND_ASYNC);
            }
        }
    }

    /// The menu for the current status, and the controller line's tooltip
    fn menu_items(&self) -> (Vec<Item>, Option<String>) {
        let s = &self.status;
        let connected = s.controller.is_some();
        let gray = |on: bool| if on { MF_STRING } else { MF_STRING | MF_GRAYED };
        let tip = match (&s.controller, &s.shunned) {
            (Some(c), _) => {
                Some(format!("MAC {}\nClick to identify, right-click to rename", shieldutl::mac::fmt_mac(&c.mac)))
            }
            // Grayed, so nothing to click
            (None, Some(sh)) => Some(format!("MAC {}", shieldutl::mac::fmt_mac(&sh.mac))),
            (None, None) => None,
        };
        let title = MF_STRING | MF_DISABLED;
        let mut items: Vec<Item> = vec![
            (MF_STRING, id::ABOUT, format!("shieldd (v{})", about_info().version)),
            (title, 0, format!(" > {} connected.", u8::from(connected))),
        ];
        if let Some(ssid) = &s.ssid {
            let ssid = menu_text(ssid);
            items.push((
                title,
                0,
                match s.channel {
                    Some(c) => format!(" > {ssid}, ch.{c}"),
                    None => format!(" > {ssid}"),
                },
            ));
        }
        items.extend([
            (MF_SEPARATOR, 0, String::new()),
            // Click to identify
            (gray(connected), id::CONTROLLER, controller_line(s.controller.as_ref(), self.pct, s.shunned.as_ref())),
            (MF_SEPARATOR, 0, String::new()),
            (
                gray(!s.pairing),
                id::PAIR,
                if connected { "Re-&pair controller..." } else { "&Pair controller..." }.into(),
            ),
            (MF_STRING, id::VBAN, if s.vban { "Disable &VBAN" } else { "Enable &VBAN" }.into()),
            (gray(connected), id::DISCONNECT, "&Disconnect".into()),
            (MF_SEPARATOR, 0, String::new()),
            (MF_STRING, id::EXIT, "E&xit".into()),
        ]);
        (items, tip)
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
                Ok(Msg::Problem(title, text)) => self.notify(&title, &text, false),
                Ok(Msg::Quit) | Err(TryRecvError::Disconnected) => return quit(),
                Err(TryRecvError::Empty) => break,
            }
        }
        if READD_WANTED.replace(false) {
            self.shown = None;
            self.refresh();
        } else if self.gauge.look_due(Instant::now()) {
            self.refresh();
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
        let stale = MENU_SHOWN.with_borrow(|shown| shown.as_ref().is_some_and(|items| *items != self.menu_items().0));
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
    let prompt = format!("Name for {}, up to {MAX_NAME} characters:", shieldutl::mac::fmt_mac(&c.mac));
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
    shieldutl::names::clean(&String::from_utf16_lossy(&text))
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
    text(&mut w, "shieldd: rename controller");
    w.push(9);
    text(&mut w, "Segoe UI");
    item(&mut w, 0, [7, 7, 206, 9], 0xFFFF, STATIC, prompt);
    let edit = (WS_BORDER | WS_TABSTOP).0 | ES_AUTOHSCROLL as u32;
    item(&mut w, edit, [7, 19, 206, 14], RENAME_EDIT as u16, EDIT, "");
    item(&mut w, WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32, [109, 41, 50, 14], IDOK.0 as u16, BUTTON, "OK");
    item(&mut w, WS_TABSTOP.0 | BS_PUSHBUTTON as u32, [163, 41, 50, 14], IDCANCEL.0 as u16, BUTTON, "Cancel");
    if w.len() % 2 == 1 {
        w.push(0);
    }
    w.chunks(2).map(|c| u32::from(c[0]) | u32::from(c[1]) << 16).collect()
}

/// Version, build, author, and the notices the GPL and the bundled works' licenses ask for
fn about(hwnd: HWND) {
    let info = about_info();
    let author = info.authors.replace(':', ", ");
    let text = format!(
        "shieldd v{version}\n\
         Built {built}\n\
         Copyright (C) 2026 {author}\n\n\
         Native Windows host for the 2015 NVIDIA SHIELD controller.\n\n\
         shieldd comes with ABSOLUTELY NO WARRANTY. It is free software, and you are welcome to \
         redistribute it under the terms of the GNU General Public License, version 2 only.\n\n\
         Includes code derived from rtw88, (c) 2018-2024 Realtek Corporation, from Realtek's \
         RTL8812AU/8821AU driver and its halmac layer, (c) Realtek Corporation, and from ozwpan, \
         (c) 2011 Ozmo Inc., all under GPL-2.0. Embeds Realtek's RTL8821A firmware, (c) 2010 Realtek \
         Semiconductor Corporation, unmodified under its own redistribution license, provided AS IS \
         without warranty. Tray icons from Ikono Bold Line Icons, (c) Will Kelly, and Clarity, (c) VMware, \
         Inc., both MIT. Rust crates under MIT, Apache-2.0, BSD and similar licenses.\n\n\
         Full license texts and copyright notices are in LICENSE and THIRD_PARTY.md, which come with \
         the source.\n\n\
         Most of shieldd was written by an AI model, Anthropic's Claude, under its maintainer's \
         direction and testing.",
        version = info.version,
        built = info.built,
    );
    let text = wide(&text);
    // SAFETY: text outlives the call
    unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            w!("About shieldd"),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        )
    };
}

/// Pairing throws the current group away, so ask first
fn confirm_pair(hwnd: HWND, connected: bool) -> bool {
    let dropped =
        if connected { "Re-pairing controllers will drop the connected controller from the network.\n" } else { "" };
    let text = format!(
        "{dropped}Once a new controller is paired, the network will be updated with a new name and passphrase. \
         Any controller paired to the old network will need to be paired again.\n\
         If no controller pairs within 2 minutes, the changes will be discarded. Any dropped controllers will \
         automatically reconnect.\n\nContinue pairing?"
    );
    let text = wide(&text);
    let title = if connected { w!("shieldd: re-pair controller") } else { w!("shieldd: pair controller") };
    // SAFETY: text outlives the call
    let r = unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            title,
            MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND,
        )
    };
    r == IDOK
}

//endregion

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
        // target/<profile>/deps/shieldtray-<hash>.exe
        exe.ancestors().nth(3).unwrap().join("tests").join("icons")
    }

    /// Every icon in both themes, on its taskbar color, for eyeballing: one PNG each, a strip
    /// of the status icons and one of the pairing animation
    #[test]
    fn renders_icons_to_target() {
        const SIZE: u32 = 64;
        const GAP: u32 = 8;
        let dir = icon_dir();
        for (theme, p, bg) in [("light", &LIGHT, [0xEE, 0xEE, 0xEE]), ("dark", &DARK, [0x20, 0x20, 0x20])] {
            let out = dir.join(theme);
            std::fs::create_dir_all(&out).unwrap();
            let bg = resvg::tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], 0xFF);
            let strip_of = |n: u32| {
                let mut s = Pixmap::new(n * SIZE + (n - 1) * GAP, SIZE).unwrap();
                s.fill(resvg::tiny_skia::Color::from_rgba8(0x80, 0x80, 0x80, 0xFF));
                s
            };
            let statuses = Icon::FIXED.iter().filter(|i| !matches!(i, Icon::Pairing(_))).count() as u32;
            let mut status_strip = strip_of(statuses);
            let mut strip = strip_of(u32::from(PAIRING_FRAMES));
            // Every eighth of a turn
            let spun = SPINNER_FRAMES / 8;
            let mut spinner_strip = strip_of(8);
            let mut next_status = 0;
            for icon in Icon::all() {
                let mut pm = Pixmap::new(SIZE, SIZE).unwrap();
                pm.fill(bg);
                let fg = icon.pixmap(p, SIZE).unwrap();
                let paint = resvg::tiny_skia::PixmapPaint::default();
                pm.draw_pixmap(0, 0, fg.as_ref(), &paint, Transform::identity(), None);
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
                let name = format!("{icon:?}").to_lowercase().replace('(', "_").replace(')', "");
                pm.save_png(out.join(format!("{name}.png"))).unwrap();
                s.draw_pixmap((i * (SIZE + GAP)) as i32, 0, pm.as_ref(), &paint, Transform::identity(), None);
            }
            status_strip.save_png(out.join("status_strip.png")).unwrap();
            strip.save_png(out.join("pairing_strip.png")).unwrap();
            spinner_strip.save_png(out.join("spinner_strip.png")).unwrap();
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
    fn shunned_line() {
        assert_eq!(controller_line(None, None, None), "No controllers connected");
        let sh = |name: Option<&str>| Shunned { mac: [0; 6], name: name.map(Into::into) };
        assert_eq!(controller_line(None, None, Some(&sh(Some("nVidiaBlake")))), "nVidiaBlake (shunned)");
        assert_eq!(controller_line(None, None, Some(&sh(None))), "Controller (shunned)");
        assert_eq!(controller_line(None, None, Some(&sh(Some("Pad&Co")))), "Pad&&Co (shunned)");
        // A connected one wins
        let c = Connected { name: Some("Pad&Co".into()), port: Some(4), ..Default::default() };
        assert_eq!(controller_line(Some(&c), None, Some(&sh(None))), "Pad&&Co: Port #4");
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
