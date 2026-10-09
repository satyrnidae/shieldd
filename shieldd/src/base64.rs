//! Standard base64 w/ padding, RFC 4648. Short enough not to need a crate.

use anyhow::{Result, bail};

const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = u32::from(c[0]) << 16 | u32::from(*c.get(1).unwrap_or(&0)) << 8 | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn decode(s: &str) -> Result<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        bail!("length isn't a multiple of 4");
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (q, c) in s.chunks(4).enumerate() {
        let pad = c.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && q + 1 != s.len() / 4) {
            bail!("misplaced padding");
        }
        let mut n = 0u32;
        for &b in &c[..4 - pad] {
            let Some(v) = ABC.iter().position(|&a| a == b) else { bail!("{:?} isn't base64", b as char) };
            n = n << 6 | v as u32;
        }
        n <<= 6 * pad;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc4648() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert!(decode("Zm9").is_err() && decode("Zg==Zm9v").is_err() && decode("Zm9-").is_err());
    }
}
