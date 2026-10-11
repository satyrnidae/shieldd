//! Replacing a file without ever leaving it half written: stage the new contents next to it, then
//! commit them in one rename

#[cfg(target_os = "windows")]
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `contents` to `<name>.tmp` next to `path`, its folder made if need be. Returns the temp file
/// for commit.
pub fn stage(path: &Path, contents: impl AsRef<[u8]>) -> Result<PathBuf> {
    stage_as(path, contents.as_ref(), false)
}

/// As stage, for a file with a key in it: only its owner can read it. On Windows that's the
/// settings folder's doing already, under the user's profile; elsewhere the file is made 0600.
pub fn stage_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<PathBuf> {
    stage_as(path, contents.as_ref(), true)
}

fn stage_as(path: &Path, contents: &[u8], private: bool) -> Result<PathBuf> {
    let mut tmp_name = path.file_name().with_context(|| format!("{} has no file name", path.display()))?.to_owned();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write(&tmp, contents, private).with_context(|| format!("writing {}", tmp.display()))?;
    Ok(tmp)
}

#[cfg(target_os = "linux")]
fn write(path: &Path, contents: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if !private {
        return std::fs::write(path, contents);
    }
    // The mode only takes on a new file, so not one left over from an earlier try
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(contents)
}

#[cfg(not(target_os = "linux"))]
fn write(path: &Path, contents: &[u8], _private: bool) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// The staged file takes `path`'s place
pub fn commit(tmp: &Path, path: &Path) -> Result<()> {
    std::fs::rename(tmp, path).with_context(|| format!("moving {} to {}", tmp.display(), path.display()))
}

/// `dir/<base>.<exts[0]>`, or `<base>-2.<exts[0]>` and on: the first base free under every one of
/// `exts`, so a name stays free whether or not its file's been gzipped yet
pub fn free_name(dir: &Path, base: &str, exts: &[&str]) -> PathBuf {
    (1..)
        .map(|n| if n == 1 { base.to_owned() } else { format!("{base}-{n}") })
        .find(|b| exts.iter().all(|x| !dir.join(format!("{b}.{x}")).exists()))
        .map(|b| dir.join(format!("{b}.{}", exts[0])))
        .expect("some name is free")
}

/// `contents` gzipped, staged as stage does
#[cfg(feature = "gzip")]
pub fn stage_gz(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    stage(path, gzip(contents)?)
}

/// `contents` gzipped, staged as stage_private does
#[cfg(feature = "gzip")]
pub fn stage_gz_private(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    stage_private(path, gzip(contents)?)
}

#[cfg(feature = "gzip")]
fn gzip(contents: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(contents)?;
    Ok(gz.finish()?)
}

/// `text`, a wpa_supplicant conf, with the plain `psk=` value at `at` swapped for `psk` sealed
#[cfg(target_os = "windows")]
pub fn seal_text(text: &str, at: Range<usize>, psk: &crate::psk::Psk) -> Result<String> {
    Ok(format!("{}{}{}", &text[..at.start], psk.seal()?, &text[at.end..]))
}

/// seal_text, written to `to`
#[cfg(target_os = "windows")]
pub fn seal_file(to: &Path, text: &str, at: Range<usize>, psk: &crate::psk::Psk) -> Result<()> {
    commit(&stage_private(to, seal_text(text, at, psk)?)?, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_dodge_every_form() {
        let dir = std::env::temp_dir().join(format!("shdlib-names-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exts = ["log", "log.gz"];
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s.log"));
        std::fs::File::create(dir.join("s.log.gz")).unwrap();
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s-2.log"));
        std::fs::File::create(dir.join("s-2.log")).unwrap();
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s-3.log"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_files_are_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("shdlib-private-test-{}", std::process::id()));
        let (open, private) = (dir.join("open"), dir.join("private"));
        commit(&stage(&open, "a").unwrap(), &open).unwrap();
        // A leftover temp file from before, readable by anyone, doesn't lend it its mode
        std::fs::write(dir.join("private.tmp"), "old").unwrap();
        commit(&stage_private(&private, "key").unwrap(), &private).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&private), 0o600);
        assert_ne!(mode(&open), 0o600);
        assert_eq!(std::fs::read_to_string(&private).unwrap(), "key");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzipped_round_trip() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("shdlib-files-test-{}", std::process::id()));
        let path = dir.join("sub").join("x.gz");
        commit(&stage_gz(&path, b"hello").unwrap(), &path).unwrap();
        let mut back = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap()).read_to_string(&mut back).unwrap();
        assert_eq!(back, "hello");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
