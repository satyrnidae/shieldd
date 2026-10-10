//! Small pieces the other crates share

#[cfg(target_os = "linux")]
#[cfg(feature = "alsa")]
pub mod alsa;
pub mod base64;
pub mod civil;
#[cfg(windows)]
pub mod dpapi;
pub mod files;
#[cfg(feature = "logs")]
pub mod logs;
pub mod names;
pub mod psk;
#[cfg(target_os = "linux")]
#[cfg(feature = "pulse")]
pub mod pulse;
#[cfg(feature = "vban")]
pub mod vban;

use std::path::PathBuf;
use std::sync::atomic::{Ordering, compiler_fence};

/// The user's local settings folder: `%LOCALAPPDATA%\shieldd` on Windows, local rather than
/// roaming since the group is tied to this machine's adapter, and `$XDG_CONFIG_HOME/shieldd`
/// or `~/.config/shieldd` elsewhere. Empty if neither is set, which leaves paths relative.
pub fn settings_dir() -> PathBuf {
    use std::env::var_os;
    #[cfg(windows)]
    let dir = var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let dir = var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    dir.map_or_else(PathBuf::new, |d| d.join("shieldd"))
}

/// Zeros `b` in a way the optimizer can't drop, for a secret on its way out
pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: x is a valid, exclusive reference to one byte
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    #[test]
    fn wipe_zeros() {
        let mut b = *b"s3cretPW";
        super::wipe(&mut b);
        assert_eq!(b, [0; 8]);
    }
}
