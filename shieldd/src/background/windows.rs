//! Running in the background on Windows: the single instance is a named event, which --stop
//! sets, and --install starts shieldw through the user's Run key.

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tracing::info;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::{PCWSTR, w};

use super::{STOP_WAIT, login_args};
use crate::Args;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("shieldd");
/// Held by whichever instance owns the adapter; set by --stop
const STOP_EVENT: PCWSTR = w!(r"Local\shieldd-stop");

/// Claims the single-instance event and returns the quit flag: set by --stop, and by ctrl+c
/// when there's a console
pub fn claim(console: bool) -> Result<Arc<AtomicBool>> {
    // SAFETY: plain named event; the handle lives as long as the process, on purpose
    let event = unsafe { CreateEventW(None, true, false, STOP_EVENT) }.context("stop event")?;
    // SAFETY: right after the call it describes
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        bail!("shieldd is already running, maybe as shieldw; `shieldd --stop` stops it");
    }
    let quit = Arc::new(AtomicBool::new(false));
    let flag = quit.clone();
    // HANDLE isn't Send; the event outlives the thread anyway
    let raw = event.0 as usize;
    std::thread::Builder::new().name("stop".into()).spawn(move || {
        // SAFETY: the event above, never closed
        unsafe { WaitForSingleObject(HANDLE(raw as *mut _), INFINITE) };
        info!("asked to stop");
        flag.store(true, Ordering::Relaxed);
    })?;
    if console {
        let flag = quit.clone();
        ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed)).context("installing ctrl+c handler")?;
    }
    Ok(quit)
}

pub fn stop() -> Result<()> {
    let Some(event) = open_stop_event() else {
        info!("nothing running");
        return Ok(());
    };
    // SAFETY: an open event handle, closed right after
    unsafe {
        SetEvent(event).context("signal stop")?;
        let _ = CloseHandle(event);
    }
    // The event goes away with the last handle, the instance's
    let mut waited = Duration::ZERO;
    while waited < STOP_WAIT {
        std::thread::sleep(Duration::from_millis(100));
        waited += Duration::from_millis(100);
        match open_stop_event() {
            // SAFETY: just opened
            Some(e) => unsafe {
                let _ = CloseHandle(e);
            },
            None => {
                info!("stopped");
                return Ok(());
            }
        }
    }
    bail!("still running after {STOP_WAIT:?}")
}

fn open_stop_event() -> Option<HANDLE> {
    // SAFETY: plain named event lookup
    unsafe { OpenEventW(EVENT_MODIFY_STATE, false, STOP_EVENT) }.ok()
}

/// shieldw, next to this shieldd, which --install starts at login
fn shieldw() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let shieldw = exe.with_file_name("shieldw.exe");
    anyhow::ensure!(shieldw.is_file(), "no {} next to {}; build both", shieldw.display(), exe.display());
    Ok(shieldw)
}

pub fn install(args: &Args) -> Result<()> {
    let exe = shieldw()?;
    let mut cmd = quote(exe.as_os_str())?;
    for a in login_args(args)? {
        cmd.push(' ');
        cmd.push_str(&quote(&a)?);
    }
    let wide: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
    // SAFETY: wide is a NUL-terminated string and its size in bytes
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    }
    .ok()
    .context("writing the Run key")?;
    info!(command = cmd, "shieldw starts at login now; run that command to start it right away");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    // SAFETY: constant strings
    let e = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
    if e == ERROR_FILE_NOT_FOUND {
        info!("wasn't set to start at login");
        return Ok(());
    }
    e.ok().context("deleting the Run key value")?;
    info!("won't start at login anymore; `shieldd --stop` stops the one running now");
    Ok(())
}

/// One argument for a Windows command line. Quotes break the simple rules, so they're refused.
fn quote(a: &OsStr) -> Result<String> {
    let a = a.to_str().context("non-Unicode argument")?;
    ensure!(!a.contains('"'), "argument with a quote in it: {a}");
    if !a.is_empty() && !a.contains([' ', '\t']) {
        return Ok(a.to_owned());
    }
    // Backslashes right before the closing quote would escape it
    let tail = a.len() - a.trim_end_matches('\\').len();
    Ok(format!("\"{a}{}\"", "\\".repeat(tail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote(OsStr::new("--mic")).unwrap(), "--mic");
        assert_eq!(quote(OsStr::new(r"C:\a b\c.exe")).unwrap(), r#""C:\a b\c.exe""#);
        assert_eq!(quote(OsStr::new(r"C:\a b\")).unwrap(), r#""C:\a b\\""#);
        assert_eq!(quote(OsStr::new("")).unwrap(), r#""""#);
        assert!(quote(OsStr::new(r#"a"b"#)).is_err());
    }
}
