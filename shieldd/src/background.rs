//! Running in the background: one instance at a time, `--stop`, and `--install`, which starts
//! shieldw at login. A logon start rather than a service, since a service's session can't move
//! the mouse, press keys or show a tray icon.
//!
//! How is per platform: a named event and the Run key on Windows (windows.rs); a lock file,
//! SIGTERM and an XDG autostart entry on Linux, where --install also puts in the udev rule and
//! so needs root (linux.rs).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, ensure};

#[cfg(target_os = "linux")]
pub use linux::{claim, install, install_udev_rule, stop, uninstall};
#[cfg(windows)]
pub use windows::{claim, install, stop, uninstall};

use crate::Args;

/// How long --stop waits for the instance to let go
const STOP_WAIT: Duration = Duration::from_secs(10);

/// shieldw, next to this shieldd, which --install starts at login
fn shieldw() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let shieldw = exe.with_file_name(format!("shieldw{}", std::env::consts::EXE_SUFFIX));
    ensure!(shieldw.is_file(), "no {} next to {}; build both", shieldw.display(), exe.display());
    Ok(shieldw)
}

/// What shieldw starts with at login: this command line, less --install
fn login_args(args: &Args) -> Result<Vec<OsString>> {
    ensure!(args.conf.is_absolute(), "give --conf as an absolute path; login starts it elsewhere");
    Ok(std::env::args_os().skip(1).filter(|a| a != "--install").collect())
}
