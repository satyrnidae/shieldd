//! Replacing a file w/o ever leaving it half written: stage the new contents next to it, then
//! commit them in one rename

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `contents` to `<name>.tmp` next to `path`, its folder made if need be. Returns the temp file
/// for commit.
pub fn stage(path: &Path, contents: impl AsRef<[u8]>) -> Result<PathBuf> {
    let mut tmp_name = path.file_name().with_context(|| format!("{} has no file name", path.display()))?.to_owned();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    Ok(tmp)
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
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(contents)?;
    stage(path, gz.finish()?)
}

/// `text`, a wpa_supplicant conf, w/ the plain `psk=` value at `at` swapped for `psk` sealed
#[cfg(windows)]
pub fn seal_text(text: &str, at: Range<usize>, psk: &crate::psk::Psk) -> Result<String> {
    Ok(format!("{}{}{}", &text[..at.start], psk.seal()?, &text[at.end..]))
}

/// seal_text, written to `to`
#[cfg(windows)]
pub fn seal_file(to: &Path, text: &str, at: Range<usize>, psk: &crate::psk::Psk) -> Result<()> {
    commit(&stage(to, seal_text(text, at, psk)?)?, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_dodge_every_form() {
        let dir = std::env::temp_dir().join(format!("shieldutl-names-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exts = ["log", "log.gz"];
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s.log"));
        std::fs::File::create(dir.join("s.log.gz")).unwrap();
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s-2.log"));
        std::fs::File::create(dir.join("s-2.log")).unwrap();
        assert_eq!(free_name(&dir, "s", &exts), dir.join("s-3.log"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzipped_round_trip() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("shieldutl-files-test-{}", std::process::id()));
        let path = dir.join("sub").join("x.gz");
        commit(&stage_gz(&path, b"hello").unwrap(), &path).unwrap();
        let mut back = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap()).read_to_string(&mut back).unwrap();
        assert_eq!(back, "hello");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
