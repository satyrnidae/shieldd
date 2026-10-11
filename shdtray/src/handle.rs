//! The tray as shieldd holds it: a handle to the tray thread, which the platform runs. Status
//! goes one way and menu picks come back the other.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use tracing::warn;

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
}

impl Tray {
    pub fn start(about: About) -> Tray {
        let _ = crate::menu::ABOUT.set(about);
        let (tx, rx) = mpsc::channel();
        let (cmd_tx, commands) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                if let Err(e) = platform::run(rx, cmd_tx) {
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
            platform::wake();
        }
    }

    /// A notification without the alarm, for a menu command that failed
    pub fn problem(&self, title: &str, text: &str) {
        let _ = self.tx.send(Msg::Problem(title.into(), text.into()));
        platform::wake();
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
        platform::shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
