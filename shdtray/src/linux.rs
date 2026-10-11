//! The Linux tray: the icon as a StatusNotifierItem (ksni), which KDE, XFCE and the rest show
//! natively and GNOME through its AppIndicator extension, with its menu over dbusmenu.
//! Notifications go over org.freedesktop.Notifications, the boxes are GTK 4 on a thread of their
//! own, and sounds are the sound theme's, through libcanberra if it's there. What it shows comes
//! from model.rs, menu.rs and icons.rs, as on Windows.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gtk4::{self as gtk, gio, glib, prelude::*};
use ksni::blocking::TrayMethods;
use ksni::menu::{StandardItem, SubMenu};
use resvg::tiny_skia::Pixmap;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

use crate::handle::Msg;
use crate::icons::{BADGES, DARK, Icon, LIGHT, Palette, rasterize, signal_svg};
use crate::menu::{self, Dialogs, Item, Kind};
use crate::model::{Model, Notice};
use crate::{Command, Signal, Status, tr};
use shdlib::names::MAX_NAME;

//region Icons

/// The sides the icon is rendered at; the panel picks the one that suits it
const SIZES: [u32; 2] = [24, 48];
/// The menu's signal bars, which the menu scales to its own size
const BAR_SIZE: u32 = 32;

/// `pm` as ksni has an icon: ARGB32 in network byte order, straight alpha
fn argb(pm: &Pixmap) -> ksni::Icon {
    let data = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.alpha(), c.red(), c.green(), c.blue()]
        })
        .collect();
    ksni::Icon { width: pm.width() as i32, height: pm.height() as i32, data }
}

/// `icon`, with its count badge if any, at every one of SIZES
fn render(icon: Icon, badge: Option<u8>, p: &Palette) -> Result<Vec<ksni::Icon>> {
    SIZES.iter().map(|&size| icon.badged(p, size, badge).map(|pm| argb(&pm))).collect()
}

/// Each icon and count badge, as rendered
type Icons = HashMap<(Icon, Option<u8>), Vec<ksni::Icon>>;

/// Every icon, with each count badge it takes, for a `light` or dark panel
fn render_all(light: bool) -> Result<Icons> {
    let palette = if light { &LIGHT } else { &DARK };
    let mut icons = HashMap::new();
    for icon in Icon::all() {
        let badges = BADGES.map(Some).filter(|_| icon.takes_badge());
        for badge in std::iter::once(None).chain(badges) {
            icons.insert((icon, badge), render(icon, badge, palette)?);
        }
    }
    debug!(light, "tray icons rendered");
    Ok(icons)
}

/// The signal bars for each level, as PNGs for the menu
fn signal_pngs() -> HashMap<Signal, Vec<u8>> {
    [Signal::Dropping, Signal::Low, Signal::Fair, Signal::Good]
        .into_iter()
        .filter_map(|s| {
            let png = rasterize(&signal_svg(s), BAR_SIZE).and_then(|pm| pm.encode_png().context("encoding png"));
            png.inspect_err(|e| warn!("signal bars: {e:#}")).ok().map(|png| (s, png))
        })
        .collect()
}

/// Whether the panel is light. GNOME's top bar is dark whatever the theme; elsewhere the panel
/// follows the desktop's color scheme, as the settings portal has it. With no preference, light.
fn light_panel(bus: Option<&Connection>) -> bool {
    let gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "GNOME"));
    if gnome {
        return false;
    }
    // 0 no preference, 1 dark, 2 light
    bus.and_then(color_scheme) != Some(1)
}

/// The portal's org.freedesktop.appearance color-scheme
fn color_scheme(bus: &Connection) -> Option<u32> {
    let reply = bus
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "ReadOne",
            &("org.freedesktop.appearance", "color-scheme"),
        )
        .inspect_err(|e| debug!("color scheme: {e}"))
        .ok()?;
    let v: OwnedValue = reply.body().deserialize().ok()?;
    match &*v {
        Value::U32(n) => Some(*n),
        Value::Value(inner) => match **inner {
            Value::U32(n) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

//endregion

//region Sounds

/// libcanberra's context and its play call, if the library's there
struct Canberra {
    ctx: *mut c_void,
    play: unsafe extern "C" fn(*mut c_void, u32, ...) -> c_int,
}

// SAFETY: libcanberra contexts lock themselves; the pointer is only used behind the Mutex anyway
unsafe impl Send for Canberra {}

static CANBERRA: OnceLock<Option<Mutex<Canberra>>> = OnceLock::new();

/// Opens libcanberra at run time, so a system without it is just silent
fn canberra() -> Option<Canberra> {
    // SAFETY: the symbols are looked up by their C names and called with their C signatures; the
    // library stays loaded for the life of the process
    unsafe {
        let lib = libc::dlopen(c"libcanberra.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            debug!("no libcanberra; no sounds");
            return None;
        }
        let create = libc::dlsym(lib, c"ca_context_create".as_ptr());
        let play = libc::dlsym(lib, c"ca_context_play".as_ptr());
        if create.is_null() || play.is_null() {
            return None;
        }
        type Create = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
        type Play = unsafe extern "C" fn(*mut c_void, u32, ...) -> c_int;
        let create = std::mem::transmute::<*mut c_void, Create>(create);
        let mut ctx = std::ptr::null_mut();
        if create(&mut ctx) != 0 || ctx.is_null() {
            return None;
        }
        Some(Canberra { ctx, play: std::mem::transmute::<*mut c_void, Play>(play) })
    }
}

/// libcanberra's error for a sound the theme doesn't have
const CA_ERROR_NOTFOUND: c_int = -9;

/// The sound theme's sound for `event`, as freedesktop's sound naming spec names it, or else
/// `fallback`'s
fn play(event: &CStr, fallback: Option<&CStr>) {
    let Some(c) = CANBERRA.get_or_init(|| canberra().map(Mutex::new)) else { return };
    let Ok(c) = c.lock() else { return };
    let theme = sound_theme();
    for event in std::iter::once(event).chain(fallback) {
        // SAFETY: a context from ca_context_create; NULL-terminated key, value pairs of C strings
        let err = unsafe {
            (c.play)(
                c.ctx,
                0,
                c"event.id".as_ptr(),
                event.as_ptr(),
                c"application.name".as_ptr(),
                c"shieldd".as_ptr(),
                c"canberra.xdg-theme.name".as_ptr(),
                theme.as_ptr(),
                std::ptr::null::<c_char>(),
            )
        };
        match err {
            0 => return,
            CA_ERROR_NOTFOUND => debug!(?event, ?theme, "no such sound"),
            _ => return debug!(?event, err, "sound"),
        }
    }
}

/// The desktop's sound theme. libcanberra only knows GTK's setting, which is freedesktop's
/// theme unless set; KDE keeps its own, ocean unless set. Each falls back to freedesktop's.
fn sound_theme() -> CString {
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "KDE"));
    let theme = kde.then(|| kde_sound_theme().unwrap_or_else(|| "ocean".into()));
    CString::new(theme.unwrap_or_else(|| "freedesktop".into())).unwrap_or_default()
}

/// `Theme=` under `[Sounds]` in kdeglobals
fn kde_sound_theme() -> Option<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    let text = std::fs::read_to_string(dir.join("kdeglobals")).ok()?;
    let mut in_sounds = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_sounds = line == "[Sounds]";
        } else if in_sounds && let Some(t) = line.strip_prefix("Theme=").filter(|t| !t.trim().is_empty()) {
            return Some(t.trim().to_owned());
        }
    }
    None
}

/// The sound theme's device-added or device-removed sound, as for a USB device
pub(crate) fn device_sound(connected: bool) {
    play(if connected { c"device-added" } else { c"device-removed" }, None);
}

//endregion

//region Tray

/// How often to look for a color scheme change
const THEME_EVERY: Duration = Duration::from_secs(5);

/// The panel is light, as the bus thread last found
static LIGHT_PANEL: AtomicBool = AtomicBool::new(true);
/// Shutting down: no more boxes
static QUITTING: AtomicBool = AtomicBool::new(false);
/// A box is up on the dialog thread
static BUSY: AtomicBool = AtomicBool::new(false);
/// GTK is up on the dialog thread, so its main context takes calls from the others
static GTK_UP: AtomicBool = AtomicBool::new(false);

/// The tray thread waits on its channel, so there's nothing to wake
pub(crate) fn wake() {}

/// Closes whatever box is open, for Tray::drop
pub(crate) fn shutdown() {
    QUITTING.store(true, Ordering::Release);
    on_gtk(close_boxes);
}

/// The item ksni serves: what's on screen, as of the last update
struct Sni {
    icon: Vec<ksni::Icon>,
    tip: String,
    items: Vec<Item>,
    bars: HashMap<Signal, Vec<u8>>,
    /// Menu picks, by id, to the tray thread
    picks: Sender<usize>,
}

impl ksni::Tray for Sni {
    // A left click opens the menu too, as on Windows
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "shieldd".into()
    }

    fn title(&self) -> String {
        "shieldd".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = self.tip.lines().map(escape).collect::<Vec<_>>().join("<br/>");
        ksni::ToolTip { title: "shieldd".into(), description, ..Default::default() }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.items.iter().map(|i| self.item(i)).collect()
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        // It comes back with the panel; ksni registers again then
        info!(?reason, "no tray to show in");
        true
    }
}

impl Sni {
    fn item(&self, i: &Item) -> ksni::MenuItem<Self> {
        let label = mnemonics(&i.text);
        // A title line can't be picked, so it's disabled. Windows shows such a line in full
        // color, but dbusmenu has nothing for that, and an enabled line would close the menu.
        let enabled = i.kind == Kind::Normal;
        let icon_data = i.signal.and_then(|s| self.bars.get(&s)).cloned().unwrap_or_default();
        match i.kind {
            Kind::Separator => ksni::MenuItem::Separator,
            _ if !i.sub.is_empty() => {
                let submenu = i.sub.iter().map(|i| self.item(i)).collect();
                SubMenu { label, enabled, icon_data, submenu, ..Default::default() }.into()
            }
            _ => {
                let id = i.id;
                let activate = Box::new(move |t: &mut Sni| {
                    if id != 0 {
                        let _ = t.picks.send(id);
                    }
                });
                StandardItem { label, enabled, icon_data, activate, ..Default::default() }.into()
            }
        }
    }
}

/// Windows' `&` shortcut marks as dbusmenu's `_`: `&x` marks x, `&&` is a plain &, and a plain _
/// is doubled
fn mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => match chars.next() {
                Some('&') => out.push('&'),
                Some(n) => {
                    out.push('_');
                    out.push(n);
                }
                None => {}
            },
            '_' => out.push_str("__"),
            c => out.push(c),
        }
    }
    out
}

/// `s` as text in markup, which tooltips and notifications may take
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

struct State {
    model: Model,
    sni: ksni::blocking::Handle<Sni>,
    /// Notifications, to the bus thread
    notices: Sender<(Notice, String, String)>,
    /// The panel's theme the icons are rendered for
    light: bool,
    /// Every icon, with each count badge it takes
    icons: Icons,
    picks: Receiver<usize>,
    /// Picks, with the status the menu showed, to the dialog thread
    boxes: Sender<(usize, Status)>,
}

pub(crate) fn run(rx: Receiver<Msg>, commands: Sender<Command>) -> Result<()> {
    let (picks_tx, picks) = mpsc::channel();
    let (boxes, boxes_rx) = mpsc::channel();
    // Not joined: a box GTK won't close mustn't hold up shutting down
    std::thread::Builder::new().name("tray boxes".into()).spawn(move || run_boxes(boxes_rx, commands))?;
    let (notices, notices_rx) = mpsc::channel();
    std::thread::Builder::new().name("tray bus".into()).spawn(move || run_bus(notices_rx))?;
    let sni = Sni { icon: Vec::new(), tip: String::new(), items: Vec::new(), bars: signal_pngs(), picks: picks_tx };
    // The panel may not be up yet at login; the icon shows once it is
    let sni = sni.assume_sni_available(true).spawn().context("tray icon")?;
    let light = LIGHT_PANEL.load(Ordering::Acquire);
    let icons = render_all(light)?;
    let mut st = State { model: Model::new(), sni, notices, light, icons, picks, boxes };
    st.refresh();
    while st.tick(&rx) {}
    QUITTING.store(true, Ordering::Release);
    on_gtk(close_boxes);
    st.sni.shutdown().wait();
    Ok(())
}

impl State {
    /// Icon, tooltip and battery notifications for the current status
    fn refresh(&mut self) {
        let view = self.model.refresh();
        self.show(view.icon, Some(view.tip));
        for (kind, title, text) in view.alerts {
            self.notify(&title, &text, kind);
        }
    }

    fn show(&mut self, icon: Icon, tip: Option<String>) {
        let badge = self.model.badge(icon);
        let Some(pixmaps) = self.icons.get(&(icon, badge)).cloned() else { return };
        let shown = self.sni.update(move |t| {
            t.icon = pixmaps;
            if let Some(tip) = tip {
                t.tip = tip;
            }
        });
        if shown.is_some() {
            self.model.shown = Some(icon);
        } else {
            debug!("tray icon service is gone");
        }
    }

    fn notify(&self, title: &str, text: &str, kind: Notice) {
        info!(title, text, ?kind, "notification");
        // Our own sound for the alarm, rather than the server's usual one
        if matches!(kind, Notice::Alarm) {
            play(c"battery-caution", Some(c"dialog-error"));
        }
        let _ = self.notices.send((kind, title.into(), text.into()));
    }

    /// Waits for the next message or frame, then takes in what Tray sent, passes on menu picks,
    /// follows the panel's theme and steps the animation. False once it's time to quit.
    fn tick(&mut self, rx: &Receiver<Msg>) -> bool {
        let mut next = rx.recv_timeout(self.model.wait());
        loop {
            match next {
                Ok(Msg::Status(s)) => {
                    self.model.status = s;
                    self.refresh();
                }
                Ok(Msg::Problem(title, text)) => self.notify(&title, &text, Notice::Warning),
                Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => return false,
                Err(RecvTimeoutError::Timeout) => break,
            }
            next = rx.recv_timeout(Duration::ZERO);
        }
        for picked in self.picks.try_iter().collect::<Vec<_>>() {
            if BUSY.load(Ordering::Acquire) {
                // A box from the menu is up, maybe behind something; that's what to show
                on_gtk(raise_boxes);
            } else {
                BUSY.store(true, Ordering::Release);
                let _ = self.boxes.send((picked, self.model.status.clone()));
            }
        }
        if self.model.looks_due(Instant::now()) {
            self.refresh();
        }
        let light = LIGHT_PANEL.load(Ordering::Acquire);
        if light != self.light {
            self.light = light;
            // Keeping the old ones if the new won't render
            match render_all(light) {
                Ok(icons) => {
                    self.icons = icons;
                    if let Some(i) = self.model.shown {
                        self.show(i, None);
                    }
                }
                Err(e) => warn!("tray icons: {e:#}"),
            }
        }
        if let Some(next) = self.model.step() {
            self.show(next, None);
        }
        let items = self.model.menu_items();
        let _ = self.sni.update(|t| {
            if t.items != items {
                t.items = items;
            }
        });
        true
    }
}

/// The bus thread: sends notifications and follows the color scheme, so the tray thread never
/// waits on a reply. A call can take as long as the desktop does: with Plasma gone, the
/// notification server's activation waits for it to come back.
fn run_bus(rx: Receiver<(Notice, String, String)>) {
    let bus = Connection::session().inspect_err(|e| warn!("session bus: {e}")).ok();
    let mut last_theme = None::<Instant>;
    loop {
        if last_theme.is_none_or(|t| t.elapsed() >= THEME_EVERY) {
            last_theme = Some(Instant::now());
            LIGHT_PANEL.store(light_panel(bus.as_ref()), Ordering::Release);
        }
        match rx.recv_timeout(THEME_EVERY) {
            Ok((kind, title, text)) => {
                if let Some(bus) = &bus {
                    notify(bus, &title, &text, kind);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A notification over org.freedesktop.Notifications. The alarm's sound is our own.
fn notify(bus: &Connection, title: &str, text: &str, kind: Notice) {
    let (icon, urgency, quiet) = match kind {
        Notice::Info => ("dialog-information", 1u8, false),
        Notice::Warning => ("dialog-warning", 1, false),
        Notice::Alarm => ("dialog-error", 2, true),
    };
    let hints: HashMap<&str, Value> =
        HashMap::from([("urgency", Value::U8(urgency)), ("suppress-sound", Value::Bool(quiet))]);
    let body = ("shieldd", 0u32, icon, title, escape(text), Vec::<&str>::new(), hints, -1i32);
    let sent = bus.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &body,
    );
    if let Err(e) = sent {
        warn!("notification: {e}");
    }
}

//endregion

//region Boxes

thread_local! {
    /// Whether GTK came up on this thread, once it's been tried
    static GTK: Cell<Option<bool>> = const { Cell::new(None) };
    /// The open alert's, to cancel it from shutdown
    static CANCEL: RefCell<Option<gio::Cancellable>> = const { RefCell::new(None) };
}

/// The dialog thread: each pick from the menu, with whatever boxes it takes, one at a time
fn run_boxes(rx: Receiver<(usize, Status)>, commands: Sender<Command>) {
    for (picked, s) in rx {
        let boxes = Boxes;
        if let Some(cmd) = menu::command(picked, &s, &boxes) {
            debug!(?cmd, "tray menu");
            let _ = commands.send(cmd.clone());
            menu::follow_up(&cmd, &s, &boxes);
        }
        BUSY.store(false, Ordering::Release);
    }
}

/// Starts GTK on this thread the first time. False if it won't, with no display say.
fn gtk_up() -> bool {
    if let Some(up) = GTK.get() {
        return up;
    }
    glib::set_prgname(Some("shieldd"));
    glib::set_application_name("shieldd");
    let up = gtk::init().inspect_err(|e| warn!("no boxes from the tray: {e}")).is_ok();
    GTK.set(Some(up));
    GTK_UP.store(up, Ordering::Release);
    up
}

/// Runs `f` on the dialog thread, if GTK is up there
fn on_gtk(f: fn()) {
    if GTK_UP.load(Ordering::Acquire) {
        glib::MainContext::default().invoke(f);
    }
}

/// Cancels the open alert and closes any other box, on the dialog thread
fn close_boxes() {
    if let Some(c) = CANCEL.take() {
        c.cancel();
    }
    for w in gtk::Window::list_toplevels() {
        if let Ok(w) = w.downcast::<gtk::Window>() {
            w.close();
        }
    }
}

/// Brings any open box to the front, on the dialog thread
fn raise_boxes() {
    for w in gtk::Window::list_toplevels() {
        if let Ok(w) = w.downcast::<gtk::Window>() {
            w.present();
        }
    }
}

/// An alert with `buttons`, until one's picked; its index, or None if closed or cancelled
fn alert(title: &str, text: &str, buttons: &[String], default: i32, cancel: i32) -> Option<i32> {
    if !gtk_up() || QUITTING.load(Ordering::Acquire) {
        return None;
    }
    let dialog = gtk::AlertDialog::builder()
        .message(title)
        .detail(text)
        .buttons(buttons.iter().map(String::as_str).collect::<Vec<_>>())
        .default_button(default)
        .cancel_button(cancel)
        .modal(true)
        .build();
    let cancellable = gio::Cancellable::new();
    CANCEL.set(Some(cancellable.clone()));
    let main = glib::MainLoop::new(None, false);
    let picked = Rc::new(Cell::new(None));
    dialog.choose(None::<&gtk::Window>, Some(&cancellable), {
        let (main, picked) = (main.clone(), picked.clone());
        move |r| {
            picked.set(r.ok());
            main.quit();
        }
    });
    main.run();
    CANCEL.set(None);
    picked.get()
}

/// The menu's boxes, in GTK on the dialog thread
struct Boxes;

impl Dialogs for Boxes {
    fn quitting(&self) -> bool {
        QUITTING.load(Ordering::Acquire)
    }

    fn inform(&self, title: &str, text: &str) {
        alert(title, text, &[tr!("dialog-ok")], 0, 0);
    }

    fn confirm(&self, title: &str, text: &str, warn: bool) -> bool {
        let buttons = [tr!("dialog-cancel"), tr!("dialog-ok")];
        // Something that can't be undone leans to Cancel
        let default = if warn { 0 } else { 1 };
        alert(title, text, &buttons, default, 0) == Some(1)
    }

    fn ask_name(&self, title: &str, prompt: &str, name: &str) -> Option<String> {
        if !gtk_up() || QUITTING.load(Ordering::Acquire) {
            return None;
        }
        let window = gtk::Window::builder().title(title).modal(true).resizable(false).build();
        let label = gtk::Label::builder().label(prompt).xalign(0.0).wrap(true).build();
        let entry = gtk::Entry::builder().text(name).max_length(MAX_NAME as i32).activates_default(true).build();
        let cancel = gtk::Button::with_label(&tr!("dialog-cancel"));
        let ok = gtk::Button::with_label(&tr!("dialog-ok"));
        ok.add_css_class("suggested-action");
        let buttons = gtk::Box::builder().spacing(6).halign(gtk::Align::End).build();
        buttons.append(&cancel);
        buttons.append(&ok);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .width_request(360)
            .build();
        content.append(&label);
        content.append(&entry);
        content.append(&buttons);
        window.set_child(Some(&content));
        window.set_default_widget(Some(&ok));

        let main = glib::MainLoop::new(None, false);
        let picked = Rc::new(RefCell::new(None));
        ok.connect_clicked({
            let (window, entry, picked) = (window.clone(), entry.clone(), picked.clone());
            move |_| {
                picked.replace(Some(entry.text().to_string()));
                window.close();
            }
        });
        cancel.connect_clicked({
            let window = window.clone();
            move |_| window.close()
        });
        window.connect_close_request({
            let main = main.clone();
            move |_| {
                main.quit();
                glib::Propagation::Proceed
            }
        });
        window.present();
        entry.grab_focus();
        main.run();
        picked.take()
    }
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_marks() {
        assert_eq!(mnemonics("&Pair controller..."), "_Pair controller...");
        assert_eq!(mnemonics("Re&configure network"), "Re_configure network");
        assert_eq!(mnemonics("P&&A-&1"), "P&A-_1");
        assert_eq!(mnemonics("DIRECT-ab_c"), "DIRECT-ab__c");
        assert_eq!(mnemonics("trailing&"), "trailing");
    }

    #[test]
    fn info_lines_cant_be_picked() {
        let (picks, _rx) = mpsc::channel();
        let sni = Sni { icon: Vec::new(), tip: String::new(), items: Vec::new(), bars: HashMap::new(), picks };
        let s = Status {
            controllers: vec![crate::menu::tests::up(1, "A", false)],
            ssid: Some("DIRECT-ab".into()),
            ..Default::default()
        };
        // Each line's label and whether it's enabled, submenus flattened
        fn lines(items: &[ksni::MenuItem<Sni>], out: &mut Vec<(String, bool)>) {
            for i in items {
                match i {
                    ksni::MenuItem::Standard(i) => out.push((i.label.clone(), i.enabled)),
                    ksni::MenuItem::SubMenu(i) => {
                        out.push((i.label.clone(), i.enabled));
                        lines(&i.submenu, out);
                    }
                    _ => {}
                }
            }
        }
        let items: Vec<_> = crate::menu::menu_items(&s, |_| Some(50)).iter().map(|i| sni.item(i)).collect();
        let mut out = Vec::new();
        lines(&items, &mut out);
        let disabled: Vec<&str> = out.iter().filter(|(_, on)| !on).map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            disabled,
            [" > 1 connected.", " > DIRECT-ab", "MAC 01:01:01:01:01:01", "Firmware v1.96/99/3.71/0.32"]
        );
        assert!(out.iter().any(|(l, on)| l == "_Identify" && *on));
    }

    #[test]
    fn icons_are_argb() {
        let pm = Icon::Full.badged(&DARK, 32, Some(2)).unwrap();
        let icon = argb(&pm);
        assert_eq!((icon.width, icon.height, icon.data.len()), (32, 32, 32 * 32 * 4));
        // The badge's disc in the corner, opaque: A, then R, G, B, straight alpha
        let at = (28 * 32 + 28) * 4;
        let c = pm.pixel(28, 28).unwrap().demultiply();
        assert_eq!(icon.data[at..at + 4], [0xFF, c.red(), c.green(), c.blue()]);
        assert_eq!(c.alpha(), 0xFF);
        assert_eq!(render(Icon::Disconnected, None, &LIGHT).unwrap().len(), SIZES.len());
        // Plain and badged, but nothing connected takes no badge
        let all = render_all(false).unwrap();
        assert!(all.contains_key(&(Icon::Spinner(3), Some(4))) && all.contains_key(&(Icon::Disconnected, None)));
        assert!(!all.contains_key(&(Icon::Disconnected, Some(2))));
    }

    #[test]
    fn every_icon_and_badge_up_front() {
        let icons = render_all(false).unwrap();
        assert!(icons.contains_key(&(Icon::Spinner(0), Some(4))) && icons.contains_key(&(Icon::Disconnected, None)));
        assert!(!icons.contains_key(&(Icon::Disconnected, Some(2))), "nothing connected, nothing to count");
        assert!(icons.values().all(|sizes| sizes.len() == SIZES.len()));
    }

    #[test]
    fn every_signal_has_bars() {
        let pngs = signal_pngs();
        assert_eq!(pngs.len(), 4);
        assert!(pngs.values().all(|p| p.starts_with(b"\x89PNG")));
    }
}
