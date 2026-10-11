//! Running in the background on Linux: the single instance holds a lock on a file in the runtime
//! folder, with its PID in it for --stop, which sends SIGTERM. --install starts `shieldd
//! --background` at login through an XDG autostart entry, and puts in the udev rule and the uinput module that let the
//! logged-in user drive the adapter and the virtual pads. Those go in /etc, so --install and
//! --uninstall need root: run with sudo, and the autostart entry is the user's who ran it.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use shdlib::files::{commit, stage};
use tracing::{info, warn};

use super::{STOP_WAIT, login_args};
use crate::Args;

/// The udev rule: the adapter and /dev/uinput for the user at the seat
const RULE: &str = include_str!("../../res/70-shieldd.rules");
const RULE_PATH: &str = "/etc/udev/rules.d/70-shieldd.rules";
/// uinput at boot
const MODULES: &str = include_str!("../../res/modules-load.conf");
const MODULES_PATH: &str = "/etc/modules-load.d/shieldd.conf";
/// The autostart entry's name, under the user's ~/.config/autostart
const AUTOSTART: &str = "shieldd.desktop";

//region One instance

/// `$XDG_RUNTIME_DIR/shieldd.lock`, which goes with the login; the settings folder without one
fn lock_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(shdlib::settings_dir);
    dir.join("shieldd.lock")
}

/// Tries for the lock on `f`: true if it's ours now, false if another instance has it
fn try_lock(f: &File, how: libc::c_int) -> Result<bool> {
    // SAFETY: flock on a file we hold open
    if unsafe { libc::flock(f.as_raw_fd(), how | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(false),
        _ => Err(e).context("locking the instance file"),
    }
}

/// Takes the instance lock and returns the quit flag: set by --stop's SIGTERM, and by ctrl+c
pub fn claim(_console: bool) -> Result<Arc<AtomicBool>> {
    let path = lock_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !try_lock(&f, libc::LOCK_EX)? {
        bail!("shieldd is already running, maybe as shieldw; `shieldd --stop` stops it");
    }
    f.set_len(0)?;
    writeln!(f, "{}", std::process::id())?;
    // Held until the process ends, which lets go of it
    std::mem::forget(f);
    let quit = Arc::new(AtomicBool::new(false));
    let flag = quit.clone();
    // SIGTERM from --stop as well as ctrl+c, and SIGHUP when the session goes
    ctrlc::set_handler(move || {
        info!("asked to stop");
        flag.store(true, Ordering::Relaxed);
    })
    .context("installing the stop handler")?;
    Ok(quit)
}

pub fn stop() -> Result<()> {
    let path = lock_path();
    let Ok(mut f) = File::open(&path) else {
        info!("nothing running");
        return Ok(());
    };
    if try_lock(&f, libc::LOCK_SH)? {
        info!("nothing running");
        return Ok(());
    }
    let mut pid = String::new();
    f.read_to_string(&mut pid)?;
    let pid: libc::pid_t = pid.trim().parse().with_context(|| format!("no PID in {}", path.display()))?;
    // SAFETY: plain kill
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("stopping {pid}"));
    }
    let mut waited = Duration::ZERO;
    while waited < STOP_WAIT {
        std::thread::sleep(Duration::from_millis(100));
        waited += Duration::from_millis(100);
        if try_lock(&f, libc::LOCK_SH)? {
            info!("stopped");
            return Ok(());
        }
    }
    bail!("still running after {STOP_WAIT:?}")
}

//endregion

//region Install

/// Starts this shieldd with --background at login for the user who ran sudo, with the rest of this
/// command line, and puts in the udev rule and the uinput module. Not shieldw, which only Windows
/// needs: an autostart entry has no terminal to hide.
pub fn install(args: &Args) -> Result<()> {
    ensure_root("--install")?;
    let exe = std::env::current_exe()?;
    let mut login = login_args(args)?;
    if !login.iter().any(|a| a == "--background") {
        login.push("--background".into());
    }
    let exec =
        std::iter::once(exe.into_os_string()).chain(login).map(|a| exec_arg(&a)).collect::<Result<Vec<_>>>()?.join(" ");
    let user = sudo_user()?;
    install_system_files()?;
    let path = user.home.join(".config/autostart").join(AUTOSTART);
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=shieldd\nComment=SHIELD controller host\nExec={exec}\n\
         Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
    );
    write_as(&user, &path, &entry)?;
    info!(command = exec, "shieldd starts at login for {} now, from {}", user.name, path.display());
    Ok(())
}

/// Stops starting at login, and takes out the udev rule and the uinput module
pub fn uninstall() -> Result<()> {
    ensure_root("--uninstall")?;
    let user = sudo_user()?;
    let path = user.home.join(".config/autostart").join(AUTOSTART);
    match remove(&path)? {
        true => info!("won't start at login for {} anymore; `shieldd --stop` stops the one running now", user.name),
        false => info!("wasn't set to start at login for {}", user.name),
    }
    let rule = remove(Path::new(RULE_PATH))?;
    let modules = remove(Path::new(MODULES_PATH))?;
    if rule || modules {
        run("udevadm", &["control", "--reload"]);
        info!("took out {RULE_PATH} and {MODULES_PATH}");
    }
    Ok(())
}

/// The udev rule and the uinput module, without the login start
pub fn install_udev_rule() -> Result<()> {
    ensure_root("--install-udev-rule")?;
    install_system_files()
}

fn ensure_root(what: &str) -> Result<()> {
    // SAFETY: plain query
    ensure!(unsafe { libc::geteuid() } == 0, "{what} writes to /etc; run it with sudo: sudo shieldd {what}");
    Ok(())
}

/// Writes the rule and the module list, loads uinput, and has udev apply the rule now to what's
/// there, so a plugged-in adapter and a loaded uinput are the user's without a replug
fn install_system_files() -> Result<()> {
    for (path, text) in [(RULE_PATH, RULE), (MODULES_PATH, MODULES)] {
        let path = Path::new(path);
        commit(&stage(path, text)?, path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    run("modprobe", &["uinput"]);
    run("udevadm", &["control", "--reload"]);
    run("udevadm", &["trigger", "--action=change", "--subsystem-match=misc", "--sysname-match=uinput"]);
    run(
        "udevadm",
        &[
            "trigger",
            "--action=change",
            "--subsystem-match=usb",
            "--attr-match=idVendor=0bda",
            "--attr-match=idProduct=0811",
        ],
    );
    info!(
        "put in {RULE_PATH} and {MODULES_PATH}; the user logged in at the seat gets the adapter and uinput, \
         or at their next login if not right away"
    );
    Ok(())
}

/// Runs a system tool, saying so if it fails; the files are in either way
fn run(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => warn!("`{program} {}` failed: {s}", args.join(" ")),
        Err(e) => warn!("running {program}: {e}"),
    }
}

/// Removes `path`; false if it wasn't there
fn remove(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The user sudo was run as, whose login --install sets up
struct User {
    name: String,
    uid: libc::uid_t,
    gid: libc::gid_t,
    home: PathBuf,
}

fn sudo_user() -> Result<User> {
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|n| n != "root")
        .context("run it with sudo as the user who'll log in, not as root itself")?;
    let c = CString::new(name.clone())?;
    // SAFETY: getpwnam's result is read before anything else could call it
    unsafe {
        let pw = libc::getpwnam(c.as_ptr());
        ensure!(!pw.is_null(), "no user {name}");
        let home = std::ffi::CStr::from_ptr((*pw).pw_dir).to_str().context("home folder isn't UTF-8")?;
        Ok(User { name, uid: (*pw).pw_uid, gid: (*pw).pw_gid, home: PathBuf::from(home) })
    }
}

/// `text` to `path` as `user`'s, any folders it makes theirs too
fn write_as(user: &User, path: &Path, text: &str) -> Result<()> {
    let mut made = Vec::new();
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| !d.exists()) {
        made.push(d.to_owned());
        dir = d.parent();
    }
    commit(&stage(path, text)?, path)?;
    for p in made.iter().map(PathBuf::as_path).chain([path]) {
        std::os::unix::fs::chown(p, Some(user.uid), Some(user.gid))
            .with_context(|| format!("giving {} to {}", p.display(), user.name))?;
    }
    Ok(())
}

/// One argument for a desktop entry's Exec line. Quoted if the spec reserves any of its
/// characters; the ones that would need escaping inside quotes are refused, as on Windows.
fn exec_arg(a: &OsStr) -> Result<String> {
    let a = a.to_str().context("non-Unicode argument")?;
    ensure!(!a.contains(['"', '`', '$', '\\', '\n']), "argument the autostart entry can't hold: {a}");
    let a = a.replace('%', "%%");
    let reserved = [' ', '\t', '\'', '>', '<', '~', '|', '&', ';', '*', '?', '#', '(', ')'];
    Ok(if a.is_empty() || a.contains(reserved) { format!("\"{a}\"") } else { a })
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(exec_arg(OsStr::new("--mic")).unwrap(), "--mic");
        assert_eq!(exec_arg(OsStr::new("/home/a b/shieldw")).unwrap(), "\"/home/a b/shieldw\"");
        assert_eq!(exec_arg(OsStr::new("100%")).unwrap(), "100%%");
        assert_eq!(exec_arg(OsStr::new("")).unwrap(), "\"\"");
        assert!(exec_arg(OsStr::new("$HOME")).is_err());
        assert!(exec_arg(OsStr::new("a\\b")).is_err());
    }

    #[test]
    fn the_rule_covers_the_adapter_and_uinput() {
        assert!(RULE.contains("ATTR{idVendor}==\"0bda\", ATTR{idProduct}==\"0811\", TAG+=\"uaccess\""));
        assert!(RULE.contains("KERNEL==\"uinput\"") && RULE.contains("static_node=uinput"));
        assert!(MODULES.lines().any(|l| l == "uinput"));
    }

    #[test]
    fn one_instance_at_a_time() {
        let dir = std::env::temp_dir().join(format!("shieldd-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shieldd.lock");
        let a = File::create(&path).unwrap();
        let b = File::open(&path).unwrap();
        assert!(try_lock(&a, libc::LOCK_EX).unwrap());
        assert!(!try_lock(&b, libc::LOCK_SH).unwrap(), "held");
        drop(a);
        assert!(try_lock(&b, libc::LOCK_SH).unwrap(), "let go with the file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
