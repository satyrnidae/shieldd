//! The controller's saved group, in wpa_supplicant P2P config form: read for hosting, written
//! by --pair. The key is sealed with DPAPI, `psk=dpapi:<base64>`, so only this Windows user can read
//! it. A plain `psk=` in the default conf gets sealed the first time it's read; one given with
//! --conf is left as it is, since something else may read it too.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
pub use shieldutl::psk::Psk;
use tracing::{info, warn};

use shieldutl::files::{commit, free_name, seal_file, seal_text, stage, stage_gz};
use shieldutl::wipe;

#[derive(Debug, Clone)]
pub struct Group {
    pub ssid: String,
    pub psk: Psk,
    /// 2.4 GHz channel, from `frequency=`. Groups from before --pair have none: channel 1.
    pub channel: u8,
}

/// The channel the old DIRECT-0S group, and any conf without `frequency=`, lives on
const DEFAULT_CHANNEL: u8 = 1;

fn channel_of(mhz: u32) -> Option<u8> {
    (2412..=2472).contains(&mhz).then(|| ((mhz - 2407) / 5) as u8)
}

fn mhz_of(channel: u8) -> u32 {
    2407 + 5 * channel as u32
}

/// First `mode=3` (GO) network block. `seal`: a plain key gets sealed in place, the rest of the
/// file left be; if that fails, hosting goes on and it's tried again next time.
pub fn read(path: &Path, seal: bool) -> Result<Group> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let found = parse(&text).with_context(|| format!("in {}", path.display()))?;
    if seal && !found.sealed {
        match seal_file(path, &text, found.psk_at.clone(), &found.group.psk) {
            Ok(()) => info!("sealed the group key in {}", path.display()),
            Err(e) => warn!("sealing the group key in {}: {e:#}", path.display()),
        }
    } else if !found.sealed {
        info!("the group key in {} isn't sealed; only the default conf gets sealed", path.display());
    }
    Ok(found.group)
}

struct Found {
    group: Group,
    /// Where the `psk=` value sits in the text
    psk_at: Range<usize>,
    sealed: bool,
}

fn parse(text: &str) -> Result<Found> {
    let mut rest = text;
    while let Some(start) = rest.find("network={") {
        let body = &rest[start + "network={".len()..];
        let end = body.find('}').context("unterminated network block")?;
        let block = &body[..end];
        rest = &body[end..];

        // Each field's value, trimmed, with its place in text
        let field = |key: &str| {
            block.lines().find_map(|l| {
                let v = l.trim().strip_prefix(key)?.strip_prefix('=')?.trim();
                let at = v.as_ptr() as usize - text.as_ptr() as usize;
                Some((v, at..at + v.len()))
            })
        };
        if field("mode").map(|f| f.0) != Some("3") {
            continue;
        }
        let ssid = field("ssid").context("GO block has no ssid")?.0;
        let (psk, psk_at) = field("psk").context("GO block has no psk")?;
        let channel = match field("frequency") {
            Some((f, _)) => {
                let mhz = f.parse().with_context(|| format!("frequency {f:?}"))?;
                channel_of(mhz).with_context(|| format!("frequency {mhz} isn't a 2.4 GHz channel"))?
            }
            None => DEFAULT_CHANNEL,
        };
        let (psk, sealed) = Psk::parse(psk)?;
        let group = Group { ssid: ssid.trim_matches('"').to_owned(), psk, channel };
        return Ok(Found { group, psk_at, sealed });
    }
    bail!("no mode=3 network block")
}

/// Replaces `path` with just this group. An existing file is kept, gzipped, in `backup/` next to
/// it, named for it and when it was last written, UTC like the log files:
/// `backup/p2p.<stamp>.conf.gz`. `seal`, as for read: the new file's key is sealed, and a plain
/// one in the backup too, or kept plain with a warning if that fails, rather than cost the save.
/// Without it, both are written plain. Returns the backup's path, if one was made.
pub fn write(path: &Path, g: &Group, seal: bool) -> Result<Option<PathBuf>> {
    // The old file before it goes, but its backup only once the new one's in: a save that fails
    // leaves no backup, so retrying one doesn't pile them up
    let old = match std::fs::read(path) {
        Ok(bytes) => Some((bytes, backup_path(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!("reading {} to back it up: {e}; replacing it without a backup", path.display());
            None
        }
    };
    let mut new = format(g, seal)?.into_bytes();
    let staged = stage(path, &new);
    wipe(&mut new);
    commit(&staged?, path)?;

    let Some((mut old, b)) = old else { return Ok(None) };
    if seal
        && let Ok(text) = std::str::from_utf8(&old)
        && let Ok(found) = parse(text)
        && !found.sealed
    {
        match seal_text(text, found.psk_at.clone(), &found.group.psk) {
            Ok(sealed) => {
                wipe(&mut old);
                old = sealed.into_bytes();
            }
            Err(e) => warn!("sealing the backup of {}: {e:#}; keeping it plain", path.display()),
        }
    }
    let saved = stage_gz(&b, &old).and_then(|t| commit(&t, &b));
    wipe(&mut old);
    match saved {
        Ok(()) => Ok(Some(b)),
        Err(e) => {
            warn!(
                "backing up the old {} to {}: {e:#}; the new group's saved all the same",
                path.display(),
                b.display()
            );
            Ok(None)
        }
    }
}

/// `dir/p2p.conf` -> `dir/backup/p2p.<stamp>.conf.gz`, or `p2p.<stamp>-2.conf.gz` and on if
/// that's taken
fn backup_path(path: &Path) -> Result<PathBuf> {
    // Not its creation time: Windows keeps that across a staged file renamed over it
    let written = std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or_else(|_| std::time::SystemTime::now());
    let stem = path.file_stem().context("conf path has no file name")?.to_string_lossy();
    let ext = path.extension().map_or_else(|| "gz".to_owned(), |x| format!("{}.gz", x.to_string_lossy()));
    let base = format!("{stem}.{}", shieldutl::civil::stamp(written));
    Ok(free_name(&path.with_file_name("backup"), &base, &[&ext]))
}

/// The file for `g`, its key sealed or plain
fn format(g: &Group, seal: bool) -> Result<String> {
    let (psk, note) = match seal {
        true => {
            (g.psk.seal()?, "# psk is sealed with DPAPI; only the Windows user who paired can read it, on this machine.")
        }
        false => (g.psk.plain(), "# psk is plain, for wpa_supplicant and the like to read too."),
    };
    let lines = [
        "# Written by shieldd --pair: the group the controller was paired with.".to_owned(),
        note.to_owned(),
        "network={".to_owned(),
        format!("\tssid=\"{}\"", g.ssid),
        format!("\tpsk={psk}"),
        "\tproto=RSN".to_owned(),
        "\tkey_mgmt=WPA-PSK".to_owned(),
        "\tpairwise=CCMP".to_owned(),
        "\tauth_alg=OPEN".to_owned(),
        "\tmode=3".to_owned(),
        "\tdisabled=2".to_owned(),
        format!("\tfrequency={}", mhz_of(g.channel)),
        "}".to_owned(),
    ];
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN_GO: &str = "network={\n\tssid=\"DIRECT-0S\"\n\tpsk=\"pass1234\"\n\tmode=3\n}\n";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shieldd-p2pconf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn picks_go_block() {
        let conf = format!("update_config=1\nnetwork={{\n\tssid=\"home\"\n\tpsk=\"x\"\n}}\n{PLAIN_GO}");
        let f = parse(&conf).unwrap();
        assert_eq!(f.group.ssid, "DIRECT-0S");
        assert_eq!(f.group.psk, Psk::Passphrase("pass1234".into()));
        assert_eq!(f.group.channel, 1);
        assert!(!f.sealed);
        assert_eq!(&conf[f.psk_at], "\"pass1234\"");
    }

    #[test]
    fn written_group_is_sealed_and_reads_back() {
        let g =
            Group { ssid: "DIRECT-FC-organized-baboon".into(), psk: Psk::Passphrase("s3cretPW".into()), channel: 11 };
        let text = format(&g, true).unwrap();
        assert!(text.contains("frequency=2462") && text.contains("psk=dpapi:"));
        assert!(!text.contains("s3cretPW"));
        let f = parse(&text).unwrap();
        assert!(f.sealed);
        assert_eq!((f.group.ssid.as_str(), &f.group.psk, f.group.channel), ("DIRECT-FC-organized-baboon", &g.psk, 11));
    }

    #[test]
    fn reading_the_default_seals_it() {
        let dir = scratch("default");
        let path = dir.join("p2p.conf");
        let conf = format!("# mine\nnetwork={{\n\tssid=\"home\"\n\tpsk=\"homepass1\"\n}}\n{PLAIN_GO}");
        std::fs::write(&path, &conf).unwrap();
        let g = read(&path, true).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("pass1234") && after.contains("psk=dpapi:"));
        // Only the GO's key; the rest stays as it was
        assert!(after.starts_with("# mine\nnetwork={\n\tssid=\"home\"\n\tpsk=\"homepass1\"\n}\n"));
        assert_eq!(read(&path, true).unwrap().psk, g.psk);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_conf_of_its_own_is_left_plain() {
        let dir = scratch("custom");
        let path = dir.join("p2p.conf");
        std::fs::write(&path, PLAIN_GO).unwrap();
        assert_eq!(read(&path, false).unwrap().psk, Psk::Passphrase("pass1234".into()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAIN_GO);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn gunzip(path: &Path) -> String {
        use std::io::Read;
        let mut out = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn backups_are_gzipped_in_their_own_folder() {
        let dir = scratch("write");
        let path = dir.join("p2p.conf");
        std::fs::write(&path, PLAIN_GO).unwrap();
        let g =
            Group { ssid: "DIRECT-FC-organized-baboon".into(), psk: Psk::Passphrase("s3cretPW".into()), channel: 6 };

        // The default: sealed in the backup
        let b = write(&path, &g, true).unwrap().unwrap();
        assert_eq!(b.parent().unwrap(), dir.join("backup"));
        let name = b.file_name().unwrap().to_str().unwrap();
        assert!(name.len() == "p2p.2026-10-09T14_01_04.conf.gz".len(), "{name}");
        assert!(name.starts_with("p2p.") && name.ends_with(".conf.gz"), "{name}");
        let backup = gunzip(&b);
        assert!(!backup.contains("pass1234") && backup.contains("DIRECT-0S"));
        assert_eq!(parse(&backup).unwrap().group.psk, Psk::Passphrase("pass1234".into()));
        assert_eq!(read(&path, false).unwrap().ssid, "DIRECT-FC-organized-baboon");

        // A --conf of its own: backed up as it was
        std::fs::write(&path, PLAIN_GO).unwrap();
        std::fs::remove_dir_all(dir.join("backup")).unwrap();
        let b = write(&path, &g, false).unwrap().unwrap();
        assert_eq!(gunzip(&b), PLAIN_GO);
        // And the new file plain too, for whatever else reads it
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.contains("psk=\"s3cretPW\"") && !new.contains("dpapi:"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
