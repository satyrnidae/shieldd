//! Logging for rtlusb and shieldd: tracing to the console and to `<dir>/latest.log`, earlier
//! runs gzipped next to it and named for when they started, UTC

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use tracing::{error, warn};

const LATEST: &str = "latest.log";
/// Packed logs kept; older ones go
const KEEP: usize = 20;

/// Panics, logged for the file; the console has Rust's own report of them
const PANIC: &str = "panic";

pub struct Setup<'a> {
    /// What to log without RUST_LOG
    pub default_filter: &'a str,
    /// Also to the console, colored only on a terminal, never in a file or a redirect
    pub console: bool,
    /// The log file's folder, and whether this run starts a new file or adds to the current one
    pub file: Option<(PathBuf, bool)>,
}

/// Sets up tracing. A console carries on without the file if it won't open. With the file, a panic
/// goes into it too. Returns whether the file's in use.
pub fn init(setup: Setup) -> Result<bool> {
    use std::io::IsTerminal;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| setup.default_filter.into())
        // A panic gets logged whatever RUST_LOG says
        .add_directive(format!("{PANIC}=error").parse().expect("a valid directive"));
    let console = setup.console.then(|| {
        fmt::layer()
            .with_ansi(std::io::stdout().is_terminal())
            .with_filter(tracing_subscriber::filter::filter_fn(|m| m.target() != PANIC))
    });
    // Nothing logs before the new file is in place, or it'd land in the last run's
    let (file, trouble) = match setup.file.as_ref().map(|(dir, rotate)| open(dir, *rotate)) {
        Some(Ok((f, stuck))) => (Some(fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f))), stuck),
        Some(Err(e)) if console.is_some() => (None, Some(e.context("logging to the console only"))),
        Some(Err(e)) => return Err(e),
        None => (None, None),
    };
    let in_use = file.is_some();
    tracing_subscriber::registry().with(filter).with(console).with(file).init();
    if let Some(e) = trouble {
        warn!("{e:#}");
    }
    if !in_use {
        return Ok(false);
    }
    // To the file; a console still gets Rust's own report, with the thread and any backtrace
    let report = std::panic::take_hook();
    let console = setup.console;
    std::panic::set_hook(Box::new(move |p| {
        error!(target: PANIC, "{p}");
        if console {
            report(p);
        }
    }));
    if let Some((dir, true)) = setup.file {
        // Packing a long log takes a moment; the program needn't wait on it
        let _ = std::thread::Builder::new().name("pack logs".into()).spawn(move || pack(&dir));
    }
    Ok(true)
}

/// Opens latest.log in `dir`. `rotate` starts a new one, moving the old one aside for `pack`;
/// otherwise this adds to it. Also returns why the old one couldn't move, in which case this
/// adds to it.
fn open(dir: &Path, rotate: bool) -> Result<(File, Option<anyhow::Error>)> {
    anyhow::ensure!(dir.is_absolute(), "no settings folder to keep logs in");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let latest = dir.join(LATEST);
    let append = || OpenOptions::new().create(true).append(true).open(&latest);
    // Anything but a clean move aside adds to the old one rather than emptying it
    let keep = |e: io::Error, doing: &str| -> Result<(File, Option<anyhow::Error>)> {
        let e = anyhow::Error::new(e).context(format!("{doing} {}; adding to it instead", latest.display()));
        Ok((append().with_context(|| format!("opening {}", latest.display()))?, Some(e)))
    };
    if !rotate {
        return Ok((append().with_context(|| format!("opening {}", latest.display()))?, None));
    }
    match fs::metadata(&latest) {
        Ok(meta) => {
            let started = meta.created().or_else(|_| meta.modified()).unwrap_or_else(|_| SystemTime::now());
            if let Err(e) =
                fs::rename(&latest, crate::files::free_name(dir, &crate::civil::stamp(started), &["log", "log.gz"]))
            {
                return keep(e, "moving aside");
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return keep(e, "looking at"),
    }
    let file = File::create(&latest).with_context(|| format!("creating {}", latest.display()))?;
    // Windows hands a new file the creation time of one that just left under the same name,
    // within 15 s. This run starts now, and the next one names it for that.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::FileTimesExt;
        let _ = file.set_times(fs::FileTimes::new().set_created(SystemTime::now()));
    }
    Ok((file, None))
}

/// Gzips every log moved aside, this run's predecessor and any a crash left behind, then drops
/// all but the newest KEEP
fn pack(dir: &Path) {
    let names = || fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path());
    for path in names() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name == LATEST || path.extension().is_none_or(|x| x != "log") {
            continue;
        }
        if let Err(e) = gzip(&path) {
            warn!("packing {}: {e:#}", path.display());
        }
    }
    // Named for when they started, so by name is by age
    let mut packed: Vec<_> = names().filter(|p| p.to_str().is_some_and(|s| s.ends_with(".log.gz"))).collect();
    packed.sort();
    for old in packed.iter().rev().skip(KEEP) {
        if let Err(e) = fs::remove_file(old) {
            warn!("removing {}: {e}", old.display());
        }
    }
}

/// `x.log` -> `x.log.gz`, through a .tmp so a half-done one never looks done
fn gzip(path: &Path) -> Result<()> {
    let gz = gz_name(path);
    let tmp = gz.with_extension("gz.tmp");
    let mut out = GzEncoder::new(File::create(&tmp)?, Compression::default());
    io::copy(&mut File::open(path)?, &mut out)?;
    out.finish()?.sync_all()?;
    fs::rename(&tmp, &gz)?;
    fs::remove_file(path)?;
    Ok(())
}

fn gz_name(log: &Path) -> PathBuf {
    let mut s = log.as_os_str().to_owned();
    s.push(".gz");
    s.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("shieldd-pack-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for day in 1..=KEEP {
            File::create(dir.join(format!("2026-01-{day:02}T00_00_00.log.gz"))).unwrap();
        }
        fs::write(dir.join("2026-02-01T00_00_00.log"), "last run").unwrap();
        fs::write(dir.join(LATEST), "this run").unwrap();
        pack(&dir);
        let mut left: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left.len(), KEEP + 1);
        assert_eq!(left[0], "2026-01-02T00_00_00.log.gz");
        assert_eq!(left[KEEP - 1], "2026-02-01T00_00_00.log.gz");
        assert_eq!(left[KEEP], LATEST);
        fs::remove_dir_all(&dir).unwrap();
    }
}
