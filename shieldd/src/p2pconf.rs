//! The controller's saved group, in wpa_supplicant P2P config form: read for hosting, written
//! by --pair. The key is sealed w/ DPAPI, `psk=dpapi:<hex>`, so only this Windows user can read
//! it; a plain `psk=` from an older or hand-made file gets sealed the first time it's read.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::dpapi;
use crate::files::{commit, stage};

#[derive(Debug, Clone)]
pub struct Group {
    pub ssid: String,
    pub psk: Psk,
    /// 2.4 GHz channel, from `frequency=`. Groups from before --pair have none: channel 1.
    pub channel: u8,
}

/// The group's key, as `psk=` holds it under the seal: a quoted passphrase, or the PMK itself
/// in 64 hex digits
#[derive(Clone, PartialEq, Eq)]
pub enum Psk {
    Passphrase(String),
    Pmk(rtlusb::wpa::Pmk),
}

const SEALED: &str = "dpapi:";

impl Psk {
    pub fn pmk(&self, ssid: &str) -> rtlusb::wpa::Pmk {
        match self {
            Psk::Passphrase(p) => rtlusb::wpa::pmk(p, ssid),
            Psk::Pmk(k) => *k,
        }
    }

    /// WPS hands the controller a passphrase; a bare PMK has none to give
    pub fn passphrase(&self) -> Option<&str> {
        match self {
            Psk::Passphrase(p) => Some(p),
            Psk::Pmk(_) => None,
        }
    }

    /// A `psk=` value, sealed or not
    fn parse(s: &str) -> Result<Psk> {
        let Some(sealed) = s.strip_prefix(SEALED) else { return Psk::parse_plain(s) };
        let plain = dpapi::unprotect(&unhex(sealed).context("sealed psk isn't hex")?)?;
        Psk::parse_plain(std::str::from_utf8(&plain).context("sealed psk isn't text")?)
    }

    fn parse_plain(s: &str) -> Result<Psk> {
        if let Some(p) = s.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
            anyhow::ensure!((8..=63).contains(&p.len()), "psk passphrase isn't 8-63 characters");
            return Ok(Psk::Passphrase(p.to_owned()));
        }
        anyhow::ensure!(s.len() == 64, "psk is neither a quoted passphrase nor 64 hex digits");
        let k = unhex(s).context("psk isn't hex")?;
        Ok(Psk::Pmk(k.try_into().expect("64 hex digits are 32 bytes")))
    }

    /// The sealed `psk=` value
    fn seal(&self) -> Result<String> {
        let plain = match self {
            Psk::Passphrase(p) => format!("\"{p}\""),
            Psk::Pmk(k) => crate::hex(k),
        };
        Ok(format!("{SEALED}{}", crate::hex(&dpapi::protect(plain.as_bytes())?)))
    }
}

/// Never the key itself, so a {:?} can't leak it into a log
impl std::fmt::Debug for Psk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Psk::Passphrase(_) => "Passphrase(..)",
            Psk::Pmk(_) => "Pmk(..)",
        })
    }
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(s.len().is_multiple_of(2) && s.is_ascii(), "odd length");
    (0..s.len()).step_by(2).map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?)).collect()
}

/// The channel the old DIRECT-0S group, and any conf w/o `frequency=`, lives on
const DEFAULT_CHANNEL: u8 = 1;

fn channel_of(mhz: u32) -> Option<u8> {
    (2412..=2472).contains(&mhz).then(|| ((mhz - 2407) / 5) as u8)
}

fn mhz_of(channel: u8) -> u32 {
    2407 + 5 * channel as u32
}

/// First `mode=3` (GO) network block. A plain key gets sealed in place, the rest of the file
/// left be; if that fails, hosting goes on and it's tried again next time.
pub fn read(path: &Path) -> Result<Group> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let found = parse(&text).with_context(|| format!("in {}", path.display()))?;
    if !found.sealed {
        match seal_in_place(path, &text, &found) {
            Ok(()) => info!("sealed the group key in {}", path.display()),
            Err(e) => warn!("sealing the group key in {}: {e:#}", path.display()),
        }
    }
    Ok(found.group)
}

fn seal_in_place(path: &Path, text: &str, found: &Found) -> Result<()> {
    let sealed = format!("{}{}{}", &text[..found.psk_at.start], found.group.psk.seal()?, &text[found.psk_at.end..]);
    commit(&stage(path, &sealed)?, path)
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

        // Each field's value, trimmed, w/ its place in text
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
        let group = Group { ssid: ssid.trim_matches('"').to_owned(), psk: Psk::parse(psk)?, channel };
        return Ok(Found { group, psk_at, sealed: psk.starts_with(SEALED) });
    }
    bail!("no mode=3 network block")
}

/// Replaces `path` w/ just this group. An existing file is kept next to it as
/// `<name>.<unix time>.bak`; returns that path. Written to `<name>.tmp` first, so a failed write
/// leaves `path` and the backups as they were.
pub fn write(path: &Path, g: &Group) -> Result<Option<PathBuf>> {
    let tmp = stage(path, &format(g)?)?;
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

fn format(g: &Group) -> Result<String> {
    let lines = [
        "# Written by shieldd --pair: the group the controller was paired with.".to_owned(),
        "# psk is sealed w/ DPAPI; only the Windows user who paired can read it, on this machine.".to_owned(),
        "network={".to_owned(),
        format!("\tssid=\"{}\"", g.ssid),
        format!("\tpsk={}", g.psk.seal()?),
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

    #[test]
    fn picks_go_block() {
        let conf = "update_config=1\nnetwork={\n\tssid=\"home\"\n\tpsk=\"x\"\n}\nnetwork={\n\tssid=\"DIRECT-0S\"\n\tpsk=\"pass1234\"\n\tmode=3\n\tdisabled=2\n}\n";
        let f = parse(conf).unwrap();
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
        let text = format(&g).unwrap();
        assert!(text.contains("frequency=2462") && text.contains("psk=dpapi:"));
        assert!(!text.contains("s3cretPW"));
        let f = parse(&text).unwrap();
        assert!(f.sealed);
        assert_eq!((f.group.ssid.as_str(), &f.group.psk, f.group.channel), ("DIRECT-FC-organized-baboon", &g.psk, 11));
    }

    #[test]
    fn hex_psk_is_the_pmk() {
        // IEEE 802.11 test vector: "password" on "IEEE"
        let hex = "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e";
        let conf = format!("network={{\nssid=\"IEEE\"\npsk={hex}\nmode=3\n}}\n");
        let g = parse(&conf).unwrap().group;
        assert_eq!(g.psk.pmk("IEEE"), rtlusb::wpa::pmk("password", "IEEE"));
        assert_eq!(g.psk.passphrase(), None);
        let sealed = format(&g).unwrap();
        assert!(!sealed.contains(hex));
        assert_eq!(parse(&sealed).unwrap().group.psk, g.psk);
        assert_eq!(format!("{g:?}").matches(hex).count(), 0);
        // Too short to be either
        assert!(parse("network={\nssid=\"DIRECT-0S\"\npsk=0123abcd\nmode=3\n}\n").is_err());
    }

    #[test]
    fn reading_seals_a_plain_key_in_place() {
        let dir = std::env::temp_dir().join(format!("shieldd-p2pconf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p2p.conf");
        let conf = "# mine\nnetwork={\n\tssid=\"home\"\n\tpsk=\"homepass1\"\n}\nnetwork={\n\tssid=\"DIRECT-0S\"\n\tpsk=\"pass1234\"\n\tmode=3\n}\n";
        std::fs::write(&path, conf).unwrap();
        let g = read(&path).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("pass1234") && after.contains("psk=dpapi:"));
        // Only the GO's key; the rest stays as it was
        assert!(after.starts_with("# mine\nnetwork={\n\tssid=\"home\"\n\tpsk=\"homepass1\"\n}\n"));
        assert_eq!(read(&path).unwrap().psk, g.psk);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
