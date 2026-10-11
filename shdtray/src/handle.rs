//! The tray as shieldd holds it: a handle to the tray thread, which the platform runs, or the
//! headless one with --no-tray. Status goes one way and menu picks come back the other.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use tracing::{info, warn};

use crate::headless;
#[cfg(gui)]
use crate::platform;
use crate::{About, Command, Status};

pub(crate) enum Msg {
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
    /// On screen, rather than headless
    #[cfg_attr(not(gui), allow(dead_code, reason = "always headless"))]
    shown: bool,
}

impl Tray {
    /// The tray on screen; headless in a build without one
    pub fn start(about: About) -> Tray {
        #[cfg(gui)]
        return Tray::spawn(about, true);
        #[cfg(not(gui))]
        {
            info!("no tray in this build; notifications go to the log");
            Tray::spawn(about, false)
        }
    }

    /// No tray on screen, for --no-tray: nothing's shown or picked, and notifications go to the log
    pub fn headless(about: About) -> Tray {
        info!("no tray; notifications go to the log");
        Tray::spawn(about, false)
    }

    fn spawn(about: About, shown: bool) -> Tray {
        let _ = crate::menu::ABOUT.set(about);
        let (tx, rx) = mpsc::channel();
        let (cmd_tx, commands) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                #[cfg(gui)]
                let ran = if shown { platform::run(rx, cmd_tx) } else { headless::run(rx, cmd_tx) };
                #[cfg(not(gui))]
                let ran = headless::run(rx, cmd_tx);
                if let Err(e) = ran {
                    warn!("tray icon: {e:#}");
                }
            })
            .ok();
        let mut tray = Tray { tx, commands, thread, last: None, shown };
        tray.update(Status::default());
        tray
    }

    /// The system's device connect or disconnect sound, as for a USB device; none headless
    pub fn device_sound(&self, connected: bool) {
        #[cfg(gui)]
        if self.shown {
            platform::device_sound(connected);
        }
        #[cfg(not(gui))]
        let _ = connected;
    }

    /// Has the tray thread look at the channel now
    fn wake(&self) {
        #[cfg(gui)]
        if self.shown {
            platform::wake();
        }
    }

    /// Only changes go through
    pub fn update(&mut self, s: Status) {
        if self.last.as_ref() != Some(&s) {
            self.last = Some(s.clone());
            let _ = self.tx.send(Msg::Status(s));
            self.wake();
        }
    }

    /// A notification without the alarm, for a menu command that failed
    pub fn problem(&self, title: &str, text: &str) {
        let _ = self.tx.send(Msg::Problem(title.into(), text.into()));
        self.wake();
    }

    /// Menu picks since the last call
    pub fn commands(&self) -> Vec<Command> {
        self.commands.try_iter().collect()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        // Whatever's open on the tray thread may never look at the channel
        #[cfg(gui)]
        if self.shown {
            platform::shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
