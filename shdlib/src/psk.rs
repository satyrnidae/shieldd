//! A wpa_supplicant `psk=` value: a quoted passphrase or the PMK itself in 64 hex digits, either
//! one optionally sealed with Windows DPAPI as `dpapi:<base64>`. The one parser for rtlusb and
//! shieldd both.

use anyhow::{Result, ensure};

use crate::wipe;

pub const SEALED: &str = "dpapi:";

/// Wiped when dropped. Moves and clones can still leave copies behind; this keeps the obvious
/// ones out of freed memory.
#[derive(Clone, PartialEq, Eq)]
pub enum Psk {
    Passphrase(String),
    Pmk([u8; 32]),
}

impl Psk {
    /// WPS hands a station a passphrase; a bare PMK has none to give
    pub fn passphrase(&self) -> Option<&str> {
        match self {
            Psk::Passphrase(p) => Some(p),
            Psk::Pmk(_) => None,
        }
    }

    /// A `psk=` value, sealed or not; also says which it was
    pub fn parse(s: &str) -> Result<(Psk, bool)> {
        match s.strip_prefix(SEALED) {
            Some(sealed) => Ok((unseal(sealed)?, true)),
            None => Ok((Psk::parse_plain(s)?, false)),
        }
    }

    /// An unsealed one: WPA wants a passphrase of 8-63 printable ASCII characters
    pub fn parse_plain(s: &str) -> Result<Psk> {
        if let Some(p) = s.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
            ensure!(
                (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                "a psk passphrase is 8-63 printable ASCII characters"
            );
            return Ok(Psk::Passphrase(p.to_owned()));
        }
        ensure!(
            s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
            "psk is neither a quoted passphrase nor 64 hex digits"
        );
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("checked as hex");
        }
        Ok(Psk::Pmk(k))
    }

    /// The plain `psk=` value: the passphrase quoted, or the PMK in hex
    pub fn plain(&self) -> String {
        match self {
            Psk::Passphrase(p) => format!("\"{p}\""),
            Psk::Pmk(k) => k.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// The sealed `psk=` value
    #[cfg(target_os = "windows")]
    pub fn seal(&self) -> Result<String> {
        let mut plain = self.plain().into_bytes();
        let sealed = crate::dpapi::protect(&plain);
        wipe(&mut plain);
        Ok(format!("{SEALED}{}", crate::base64::encode(&sealed?)))
    }
}

#[cfg(target_os = "windows")]
fn unseal(sealed: &str) -> Result<Psk> {
    use anyhow::Context;
    let blob = crate::base64::decode(sealed).context("sealed psk isn't base64")?;
    let mut plain = crate::dpapi::unprotect(&blob)?;
    let psk = std::str::from_utf8(&plain).context("sealed psk isn't text").and_then(Psk::parse_plain);
    wipe(&mut plain);
    psk
}

#[cfg(not(target_os = "windows"))]
fn unseal(_: &str) -> Result<Psk> {
    anyhow::bail!("psk is sealed with Windows DPAPI; only the Windows user who sealed it can open it")
}

impl Drop for Psk {
    fn drop(&mut self) {
        match self {
            // SAFETY: zeros are valid UTF-8, and the string's dropped right after
            Psk::Passphrase(p) => wipe(unsafe { p.as_bytes_mut() }),
            Psk::Pmk(k) => wipe(k),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e";

    #[test]
    fn plain_forms() {
        assert_eq!(Psk::parse("\"pass1234\"").unwrap(), (Psk::Passphrase("pass1234".into()), false));
        let (k, sealed) = Psk::parse(HEX).unwrap();
        assert!(!sealed && k.passphrase().is_none());
        assert_eq!(format!("{k:?}"), "Pmk(..)");
    }

    #[test]
    fn strict_about_what_wpa_takes() {
        // Too short, too long, not ASCII, not printable
        for bad in ["\"short\"", &format!("\"{}\"", "x".repeat(64)), "\"pässwörd1\"", "\"pass\t1234\""] {
            assert!(Psk::parse_plain(bad).is_err(), "{bad}");
        }
        // 64 characters, but not all hex: from_str_radix alone would take the +
        let plus = format!("+{}", &HEX[1..]);
        assert!(Psk::parse_plain(&plus).is_err());
        assert!(Psk::parse_plain(&HEX[2..]).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn sealed_round_trip() {
        for psk in [Psk::Passphrase("s3cretPW".into()), Psk::parse_plain(HEX).unwrap()] {
            let sealed = psk.seal().unwrap();
            assert!(sealed.starts_with(SEALED) && !sealed.contains("s3cretPW") && !sealed.contains(HEX));
            assert_eq!(Psk::parse(&sealed).unwrap(), (psk, true));
        }
        assert!(Psk::parse("dpapi:not base64!").is_err());
    }
}
