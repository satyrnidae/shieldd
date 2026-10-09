//! The log file: `settings_dir()/logs/latest.log`, earlier runs gzipped next to it and named
//! for when they started, UTC

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use tracing::warn;

const LATEST: &str = "latest.log";

pub fn dir() -> PathBuf {
    crate::settings_dir().join("logs")
}

/// Opens latest.log. `rotate` starts a new one, moving the old one aside for `pack`; otherwise
/// this adds to it. Also returns why the old one couldn't move, in which case this adds to it.
pub fn open(rotate: bool) -> Result<(File, Option<anyhow::Error>)> {
    let dir = dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let latest = dir.join(LATEST);
    let append = || OpenOptions::new().create(true).append(true).open(&latest);
    if !rotate {
        return Ok((append().with_context(|| format!("opening {}", latest.display()))?, None));
    }
    if let Ok(meta) = fs::metadata(&latest) {
        let started = meta.created().or_else(|_| meta.modified()).unwrap_or_else(|_| SystemTime::now());
        let aside = free_name(&dir, &stamp(started));
        if let Err(e) = fs::rename(&latest, &aside) {
            let e = anyhow::Error::new(e).context(format!("moving {} aside; adding to it instead", latest.display()));
            return Ok((append().with_context(|| format!("opening {}", latest.display()))?, Some(e)));
        }
    }
    let file = File::create(&latest).with_context(|| format!("creating {}", latest.display()))?;
    // Windows hands a new file the creation time of one that just left under the same name,
    // within 15 s. This run starts now, and the next one names it for that.
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;
        let _ = file.set_times(fs::FileTimes::new().set_created(SystemTime::now()));
    }
    Ok((file, None))
}

/// Gzips every log moved aside, this run's predecessor and any a crash left behind
pub fn pack() {
    let Ok(entries) = fs::read_dir(dir()) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name == LATEST || path.extension().is_none_or(|x| x != "log") {
            continue;
        }
        if let Err(e) = gzip(&path) {
            warn!("packing {}: {e:#}", path.display());
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

/// `stamp.log`, or `stamp-2.log` and on if two runs started the same second
fn free_name(dir: &Path, stamp: &str) -> PathBuf {
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{stamp}.log") } else { format!("{stamp}-{n}.log") }))
        .find(|p| !p.exists() && !gz_name(p).exists())
        .expect("some name is free")
}

/// YYYY-MM-DDTHH_mm_SS, UTC like the log's lines. Windows forbids `:` in a file name.
fn stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{}T{h:02}_{m:02}_{s:02}", crate::civil::date(secs / 86_400))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamps_are_utc_and_colonless() {
        let t = UNIX_EPOCH + Duration::from_secs(1_791_554_464);
        assert_eq!(stamp(t), "2026-10-09T14_01_04");
    }

    #[test]
    fn names_dodge_packed_and_unpacked_logs() {
        let dir = std::env::temp_dir().join(format!("shieldd-logs-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "s"), dir.join("s.log"));
        File::create(dir.join("s.log.gz")).unwrap();
        assert_eq!(free_name(&dir, "s"), dir.join("s-2.log"));
        File::create(dir.join("s-2.log")).unwrap();
        assert_eq!(free_name(&dir, "s"), dir.join("s-3.log"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
