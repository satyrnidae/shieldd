//! The controller's saved group, in wpa_supplicant P2P config form: read for hosting, written
//! by --pair.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Group {
    pub ssid: String,
    pub passphrase: String,
    /// 2.4 GHz channel, from `frequency=`. Groups from before --pair have none: channel 1.
    pub channel: u8,
}

/// The channel the old DIRECT-0S group, and any conf w/o `frequency=`, lives on
const DEFAULT_CHANNEL: u8 = 1;

fn channel_of(mhz: u32) -> Option<u8> {
    (2412..=2472).contains(&mhz).then(|| ((mhz - 2407) / 5) as u8)
}

fn mhz_of(channel: u8) -> u32 {
    2407 + 5 * channel as u32
}

/// First `mode=3` (GO) network block. The WinRT GO only takes a passphrase,
/// so a raw hex PSK is an error.
pub fn read(path: &Path) -> Result<Group> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

fn parse(text: &str) -> Result<Group> {
    let mut rest = text;
    while let Some(start) = rest.find("network={") {
        let body = &rest[start + "network={".len()..];
        let end = body.find('}').context("unterminated network block")?;
        let block = &body[..end];
        rest = &body[end..];

        let field =
            |key: &str| block.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('=').map(str::to_owned));
        if field("mode").as_deref() != Some("3") {
            continue;
        }
        let ssid = field("ssid").context("GO block has no ssid")?;
        let psk = field("psk").context("GO block has no psk")?;
        let Some(passphrase) = psk.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
            bail!("psk is a raw hex key; the WinRT GO only takes a passphrase");
        };
        let channel = match field("frequency") {
            Some(f) => {
                let mhz = f.trim().parse().with_context(|| format!("frequency {f:?}"))?;
                channel_of(mhz).with_context(|| format!("frequency {mhz} isn't a 2.4 GHz channel"))?
            }
            None => DEFAULT_CHANNEL,
        };
        return Ok(Group { ssid: ssid.trim_matches('"').to_owned(), passphrase: passphrase.to_owned(), channel });
    }
    bail!("no mode=3 network block")
}

/// Replaces `path` w/ just this group. An existing file is kept next to it as
/// `<name>.<unix time>.bak`; returns that path. Written to `<name>.tmp` first, so a failed write
/// leaves `path` and the backups as they were.
pub fn write(path: &Path, g: &Group) -> Result<Option<PathBuf>> {
    let tmp = stage(path, &format(g))?;
    let backup = if path.exists() {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let mut name = path.file_name().context("conf path has no file name")?.to_owned();
        name.push(format!(".{secs}.bak"));
        let b = path.with_file_name(name);
        std::fs::rename(path, &b).with_context(|| format!("backing up {} to {}", path.display(), b.display()))?;
        Some(b)
    } else {
        None
    };
    commit(&tmp, path)?;
    Ok(backup)
}

/// The first half of replacing `path` w/o ever leaving it half written: `contents` to
/// `<name>.tmp` next to it, its folder made if need be. Returns the temp file for commit.
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

/// The second half: the staged file takes `path`'s place
pub fn commit(tmp: &Path, path: &Path) -> Result<()> {
    std::fs::rename(tmp, path).with_context(|| format!("moving {} to {}", tmp.display(), path.display()))
}

fn format(g: &Group) -> String {
    let lines = [
        "# Written by shieldd --pair: the group the controller was paired with.".to_owned(),
        "network={".to_owned(),
        format!("\tssid=\"{}\"", g.ssid),
        format!("\tpsk=\"{}\"", g.passphrase),
        "\tproto=RSN".to_owned(),
        "\tkey_mgmt=WPA-PSK".to_owned(),
        "\tpairwise=CCMP".to_owned(),
        "\tauth_alg=OPEN".to_owned(),
        "\tmode=3".to_owned(),
        "\tdisabled=2".to_owned(),
        format!("\tfrequency={}", mhz_of(g.channel)),
        "}".to_owned(),
    ];
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_go_block() {
        let conf = "update_config=1\nnetwork={\n\tssid=\"home\"\n\tpsk=\"x\"\n}\nnetwork={\n\tssid=\"DIRECT-0S\"\n\tpsk=\"pass1234\"\n\tmode=3\n\tdisabled=2\n}\n";
        let g = parse(conf).unwrap();
        assert_eq!(g.ssid, "DIRECT-0S");
        assert_eq!(g.passphrase, "pass1234");
        assert_eq!(g.channel, 1);
    }

    #[test]
    fn written_group_reads_back() {
        let g = Group { ssid: "DIRECT-FC-organized-baboon".into(), passphrase: "s3cretPW".into(), channel: 11 };
        let text = format(&g);
        assert!(text.contains("frequency=2462"));
        let r = parse(&text).unwrap();
        assert_eq!((r.ssid.as_str(), r.passphrase.as_str(), r.channel), ("DIRECT-FC-organized-baboon", "s3cretPW", 11));
    }

    #[test]
    fn rejects_hex_psk() {
        let conf = "network={\nssid=\"DIRECT-0S\"\npsk=0123abcd\nmode=3\n}\n";
        assert!(parse(conf).is_err());
    }
}
