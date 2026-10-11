//! No tray on screen: --no-tray, or a build without one (Linux without the `tray` feature).
//! shieldd hosts the same; nothing's shown or picked, and the notifications the tray would show
//! go to the log.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use crate::Command;
use crate::handle::Msg;
use crate::model::{Model, Notice};

pub(crate) fn run(rx: Receiver<Msg>, _: Sender<Command>) -> Result<()> {
    let mut model = Model::new();
    loop {
        match rx.recv_timeout(model.wait()) {
            Ok(Msg::Status(s)) => {
                model.status = s;
                log(&model.refresh().alerts);
            }
            Ok(Msg::Problem(title, text)) => warn!(title, text, "notification"),
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if model.looks_due(Instant::now()) {
            log(&model.refresh().alerts);
        }
    }
}

fn log(alerts: &[(Notice, String, String)]) {
    for (kind, title, text) in alerts {
        match kind {
            Notice::Info => info!(title, text, "notification"),
            Notice::Warning | Notice::Alarm => warn!(title, text, ?kind, "notification"),
        }
    }
}
