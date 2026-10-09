//! Replacing a file w/o ever leaving it half written: stage the new contents next to it, then
//! commit them in one rename

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `contents` to `<name>.tmp` next to `path`, its folder made if need be. Returns the temp file
/// for commit.
pub fn stage(path: &Path, contents: &str) -> Result<PathBuf> {
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

/// Writes `text`, a wpa_supplicant conf, to `to` w/ the plain `psk=` value at `at` swapped for
/// `psk` sealed
#[cfg(windows)]
pub fn seal_file(to: &Path, text: &str, at: Range<usize>, psk: &crate::psk::Psk) -> Result<()> {
    let sealed = format!("{}{}{}", &text[..at.start], psk.seal()?, &text[at.end..]);
    commit(&stage(to, &sealed)?, to)
}
