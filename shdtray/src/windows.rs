//! The Windows tray: the icon in the taskbar's notification area, its menu and boxes, and
//! notifications as balloons. What it shows comes from model.rs, menu.rs and icons.rs; this
//! puts it on screen with Shell_NotifyIcon, colored for the taskbar's light or dark theme.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use resvg::tiny_skia::Pixmap;
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

use crate::handle::Msg;
use crate::icons::{BADGES, DARK, Icon, LIGHT, rasterize, signal_svg};
use crate::menu::{self, Dialogs, Item, Kind, without_signal};
use crate::model::{Model, Notice};
use crate::{Command, Signal, tr};
use shdlib::names::MAX_NAME;
use windows::core::{BOOL, PCWSTR, PWSTR, w};

//region Icons

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

/// Has the tray thread look at the channel now, rather than at its next tick
pub(crate) fn wake() {
    let hwnd = TRAY_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: a stale handle just fails the post
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_WAKE, WPARAM(0), LPARAM(0)) };
    }
}

/// Closes whatever's open, for Tray::drop: an open message box or menu runs its own loop and
/// never sees Quit, but it still dispatches this
pub(crate) fn shutdown() {
    let hwnd = TRAY_HWND.load(Ordering::Acquire);
    if hwnd != 0 {
        // SAFETY: a stale handle just fails the post
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), WM_SHUTDOWN, WPARAM(0), LPARAM(0)) };
    }
}

/// How often to look for a taskbar theme change
const THEME_EVERY: Duration = Duration::from_secs(5);
/// The icon's callback message
const WM_TRAY: u32 = WM_APP + 1;
/// Close whatever's open, see Tray::drop
const WM_SHUTDOWN: u32 = WM_APP + 2;
/// Something's waiting on the channel, see wake
const WM_WAKE: u32 = WM_APP + 3;
/// State::tick's timer
const TICK_TIMER: usize = 1;
/// The tray window, for Tray::drop; one per process
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

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

struct State {
    hwnd: HWND,
    rx: Receiver<Msg>,
    light: bool,
    last_theme: Instant,
    /// Every icon, with each count badge it takes
    icons: Vec<(Icon, Option<u8>, HICON)>,
    model: Model,
    commands: Sender<Command>,
}

pub(crate) fn run(rx: Receiver<Msg>, commands: Sender<Command>) -> Result<()> {
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
        model: Model::new(),
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
        let Some((items, s, commands)) =
            with_state(|st| (st.model.menu_items(), st.model.status.clone(), st.commands.clone()))
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
    let boxes = Boxes(hwnd);
    let Some(cmd) = menu::command(picked, &s, &boxes) else { return };
    debug!(?cmd, "tray menu");
    let _ = commands.send(cmd.clone());
    menu::follow_up(&cmd, &s, &boxes);
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
                true => (flags(i.kind), i.id),
                false => match build_menu(&i.sub, size) {
                    Some(sub) => (flags(i.kind) | MF_POPUP, sub.0 as usize),
                    None => (flags(i.kind) | MF_GRAYED, 0),
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

fn flags(kind: Kind) -> MENU_ITEM_FLAGS {
    match kind {
        Kind::Normal => MF_STRING,
        Kind::Title => MF_STRING | MF_DISABLED,
        Kind::Grayed => MF_STRING | MF_GRAYED,
        Kind::Separator => MF_SEPARATOR,
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
        let msg = if self.model.shown.is_some() { NIM_MODIFY } else { NIM_ADD };
        let view = self.model.refresh();
        self.show(view.icon, msg, Some(&view.tip));
        for (kind, title, text) in view.alerts {
            self.notify(&title, &text, kind);
        }
    }

    fn show(&mut self, icon: Icon, msg: NOTIFY_ICON_MESSAGE, tip: Option<&str>) {
        let badge = self.model.badge(icon);
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
            self.model.shown = Some(icon);
        } else {
            debug!("Shell_NotifyIcon failed");
        }
    }

    fn hide(&mut self) {
        if self.model.shown.take().is_some() {
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

    /// Takes in what Tray sent, follows the taskbar theme, steps the animation, flags a stale
    /// menu, and sets the timer for the next go
    fn tick(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Status(s)) => {
                    self.model.status = s;
                    self.refresh();
                }
                Ok(Msg::Problem(title, text)) => self.notify(&title, &text, Notice::Warning),
                Ok(Msg::Quit) | Err(TryRecvError::Disconnected) => return quit(),
                Err(TryRecvError::Empty) => break,
            }
        }
        if READD_WANTED.replace(false) {
            self.model.shown = None;
            self.refresh();
        } else if self.model.looks_due(Instant::now()) {
            self.refresh();
        }
        if self.last_theme.elapsed() >= THEME_EVERY {
            self.last_theme = Instant::now();
            let light = light_taskbar();
            if light != self.light {
                self.light = light;
                match self.load_icons() {
                    Ok(()) => {
                        if let Some(i) = self.model.shown {
                            self.show(i, NIM_MODIFY, None);
                        }
                    }
                    Err(e) => warn!("tray icons: {e:#}"),
                }
            }
        }
        if let Some(next) = self.model.step() {
            self.show(next, NIM_MODIFY, None);
        }
        // Not for the signal bars alone: reopening would collapse an open submenu each time a link
        // flickers. They catch up the next time the menu opens.
        let stale = MENU_SHOWN.with_borrow(|shown| {
            shown.as_ref().is_some_and(|items| *items != without_signal(&self.model.menu_items()))
        });
        if stale {
            MENU_REOPEN.set(true);
            // SAFETY: plain call; closes the menu this thread has open
            let _ = unsafe { EndMenu() };
        }
        let wait = self.model.wait();
        // SAFETY: our window, on its thread; a new SetTimer with the same ID replaces the old
        unsafe { SetTimer(Some(self.hwnd), TICK_TIMER, (wait.as_millis() as u32).max(1), None) };
    }
}

/// The menu's boxes, in front of the tray window
struct Boxes(HWND);

impl Dialogs for Boxes {
    fn quitting(&self) -> bool {
        QUITTING.get()
    }

    fn inform(&self, title: &str, text: &str) {
        message(self.0, text, title, MB_OK | MB_ICONINFORMATION);
    }

    fn confirm(&self, title: &str, text: &str, warn: bool) -> bool {
        let style = match warn {
            true => MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
            false => MB_OKCANCEL | MB_ICONINFORMATION,
        };
        message(self.0, text, title, style) == IDOK
    }

    fn ask_name(&self, title: &str, prompt: &str, name: &str) -> Option<String> {
        RENAME_TEXT.set(wide(name));
        let template = rename_template(title, prompt);
        // SAFETY: a complete template, DWORD-aligned, that outlives the call; rename_proc is 'static
        let r = unsafe {
            let instance = GetModuleHandleW(None).ok().map(Into::into);
            DialogBoxIndirectParamW(instance, template.as_ptr().cast(), Some(self.0), Some(rename_proc), LPARAM(0))
        };
        if r != IDOK.0 as isize {
            return None;
        }
        Some(String::from_utf16_lossy(&RENAME_TEXT.take()))
    }
}

/// The rename dialog's edit box
const RENAME_EDIT: i32 = 100;

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
fn rename_template(title: &str, prompt: &str) -> Vec<u32> {
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
    text(&mut w, title);
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
    use crate::Status;
    use crate::menu::menu_items;
    use crate::menu::tests::up;

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
    fn every_signal_makes_a_menu_bitmap() {
        for s in [Signal::Dropping, Signal::Low, Signal::Fair, Signal::Good] {
            let b = menu_bitmap(&rasterize(&signal_svg(s), 16).unwrap()).unwrap();
            // SAFETY: just made, used by nothing
            let _ = unsafe { DeleteObject(b.into()) };
        }
    }
}
