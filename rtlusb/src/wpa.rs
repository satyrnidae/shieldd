//! WPA2-PSK authenticator side: key derivation, EAPOL-Key frames, 4-way handshake messages.
//! IEEE 802.11-2020 12.7; hostapd's wpa_auth.c is the behavior reference.

use aes_kw::KekAes128;
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use shdlib::psk::Psk;

type HmacSha1 = Hmac<Sha1>;

pub type Pmk = [u8; 32];

/// PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)
pub fn pmk(passphrase: &str, ssid: &str) -> Pmk {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(passphrase.as_bytes(), ssid.as_bytes(), 4096, &mut out);
    out
}

/// The PMK a `psk=` value stands for
pub fn pmk_of(psk: &Psk, ssid: &str) -> Pmk {
    match psk {
        Psk::Passphrase(p) => pmk(p, ssid),
        Psk::Pmk(k) => *k,
    }
}

/// Reads `psk=` from a wpa_supplicant network block: a quoted passphrase or 64 hex digits,
/// sealed by shieldd or not
pub fn pmk_from_conf(conf: &str, ssid: &str) -> Result<Pmk> {
    let line = conf.lines().map(str::trim).find_map(|l| l.strip_prefix("psk=")).context("no psk= line")?;
    Ok(pmk_of(&Psk::parse(line.trim())?.0, ssid))
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut m = HmacSha1::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// PRF-384 for CCMP: KCK 16, KEK 16, TK 16
#[derive(Clone)]
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

pub fn ptk(pmk: &Pmk, aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> Ptk {
    let (a1, a2) = if aa < spa { (aa, spa) } else { (spa, aa) };
    let (n1, n2) = if anonce < snonce { (anonce, snonce) } else { (snonce, anonce) };
    let mut out = Vec::with_capacity(60);
    for i in 0u8..3 {
        out.extend_from_slice(&hmac_sha1(pmk, &[b"Pairwise key expansion", &[0], a1, a2, n1, n2, &[i]]));
    }
    Ptk {
        kck: out[0..16].try_into().unwrap(),
        kek: out[16..32].try_into().unwrap(),
        tk: out[32..48].try_into().unwrap(),
    }
}

//region EAPOL-Key

pub const KI_VERSION_AES: u16 = 2;
pub const KI_PAIRWISE: u16 = 1 << 3;
pub const KI_INSTALL: u16 = 1 << 6;
pub const KI_ACK: u16 = 1 << 7;
pub const KI_MIC: u16 = 1 << 8;
pub const KI_SECURE: u16 = 1 << 9;
pub const KI_ENC_DATA: u16 = 1 << 12;

/// Offsets inside the EAPOL frame, from the 802.1X version byte
const OFF_KEY_INFO: usize = 5;
const OFF_REPLAY: usize = 9;
const OFF_NONCE: usize = 17;
const OFF_MIC: usize = 81;
const OFF_DATA_LEN: usize = 97;
const KEY_HDR: usize = 99;

pub struct EapolKey<'a> {
    pub key_info: u16,
    pub replay: u64,
    pub nonce: [u8; 32],
    #[allow(dead_code)] // Msg 2 carries the STA RSN IE; not checked yet
    pub data: &'a [u8],
}

/// Parses an EAPOL-Key frame starting at the 802.1X header
pub fn parse(eapol: &[u8]) -> Option<EapolKey<'_>> {
    if eapol.len() < KEY_HDR || eapol[1] != 3 || eapol[4] != 2 {
        return None;
    }
    let data_len = u16::from_be_bytes([eapol[OFF_DATA_LEN], eapol[OFF_DATA_LEN + 1]]) as usize;
    Some(EapolKey {
        key_info: u16::from_be_bytes([eapol[OFF_KEY_INFO], eapol[OFF_KEY_INFO + 1]]),
        replay: u64::from_be_bytes(eapol[OFF_REPLAY..OFF_REPLAY + 8].try_into().unwrap()),
        nonce: eapol[OFF_NONCE..OFF_NONCE + 32].try_into().unwrap(),
        data: eapol.get(KEY_HDR..KEY_HDR + data_len)?,
    })
}

/// HMAC-SHA1-128 over the frame with the MIC field zeroed
pub fn mic(kck: &[u8; 16], eapol: &[u8]) -> [u8; 16] {
    let mut f = eapol.to_vec();
    f[OFF_MIC..OFF_MIC + 16].fill(0);
    hmac_sha1(kck, &[&f])[..16].try_into().unwrap()
}

pub fn verify_mic(kck: &[u8; 16], eapol: &[u8]) -> bool {
    eapol.len() >= KEY_HDR && mic(kck, eapol)[..] == eapol[OFF_MIC..OFF_MIC + 16]
}

/// Builds an EAPOL-Key frame, starting at the 802.1X header. MIC is computed when `kck` is given.
pub fn build(key_info: u16, replay: u64, nonce: &[u8; 32], data: &[u8], kck: Option<&[u8; 16]>) -> Vec<u8> {
    let body_len = KEY_HDR - 4 + data.len();
    let mut f = Vec::with_capacity(KEY_HDR + data.len());
    f.push(2); // 802.1X-2004
    f.push(3); // Key
    f.extend_from_slice(&(body_len as u16).to_be_bytes());
    f.push(2); // RSN descriptor
    f.extend_from_slice(&key_info.to_be_bytes());
    f.extend_from_slice(&16u16.to_be_bytes()); // CCMP key length
    f.extend_from_slice(&replay.to_be_bytes());
    f.extend_from_slice(nonce);
    f.extend_from_slice(&[0; 16]); // IV
    f.extend_from_slice(&[0; 8]); // RSC
    f.extend_from_slice(&[0; 8]); // reserved
    f.extend_from_slice(&[0; 16]); // MIC
    f.extend_from_slice(&(data.len() as u16).to_be_bytes());
    f.extend_from_slice(data);
    if let Some(kck) = kck {
        let m = mic(kck, &f);
        f[OFF_MIC..OFF_MIC + 16].copy_from_slice(&m);
    }
    f
}

/// Message 3 key data: our RSN IE and the GTK KDE, padded and AES key wrapped with the KEK
pub fn msg3_key_data(kek: &[u8; 16], rsn_ie: &[u8], gtk: &[u8; 16], gtk_idx: u8) -> Result<Vec<u8>> {
    let mut plain = rsn_ie.to_vec();
    plain.extend_from_slice(&[0xdd, 6 + 16, 0x00, 0x0f, 0xac, 0x01, gtk_idx & 0x3, 0x00]);
    plain.extend_from_slice(gtk);
    // Pad to a multiple of 8, at least 16: 0xdd then zeros
    if !plain.len().is_multiple_of(8) || plain.len() < 16 {
        plain.push(0xdd);
        while !plain.len().is_multiple_of(8) || plain.len() < 16 {
            plain.push(0);
        }
    }
    let kw: KekAes128 = KekAes128::from(*kek);
    let mut out = vec![0u8; plain.len() + 8];
    kw.wrap(&plain, &mut out).map_err(|e| anyhow::anyhow!("key wrap: {e}"))?;
    Ok(out)
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn pmk_matches_80211_test_vector() {
        // IEEE 802.11-2020 J.4.2, passphrase "password", SSID "IEEE"
        assert_eq!(
            pmk("password", "IEEE").to_vec(),
            hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e")
        );
    }

    #[test]
    fn key_wrap_matches_rfc3394() {
        let kek: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let kw = KekAes128::from(kek);
        let mut out = [0u8; 24];
        kw.wrap(&hex("00112233445566778899aabbccddeeff"), &mut out).unwrap();
        assert_eq!(out.to_vec(), hex("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"));
    }

    #[test]
    fn built_frame_parses_and_mic_verifies() {
        let kck = [7u8; 16];
        let nonce = [9u8; 32];
        let f = build(KI_VERSION_AES | KI_PAIRWISE | KI_ACK | KI_MIC, 2, &nonce, &[1, 2, 3], Some(&kck));
        let k = parse(&f).unwrap();
        assert_eq!(k.replay, 2);
        assert_eq!(k.nonce, nonce);
        assert_eq!(k.data, &[1, 2, 3]);
        assert!(verify_mic(&kck, &f));
        assert!(!verify_mic(&[8u8; 16], &f));
        assert_eq!(u16::from_be_bytes([f[2], f[3]]) as usize, f.len() - 4);
    }

    #[test]
    fn ptk_is_symmetric_in_address_and_nonce_order() {
        let pmk = [1u8; 32];
        let (a, s) = ([2u8; 6], [3u8; 6]);
        let (an, sn) = ([4u8; 32], [5u8; 32]);
        assert_eq!(ptk(&pmk, &a, &s, &an, &sn).tk, ptk(&pmk, &s, &a, &sn, &an).tk);
    }

    #[test]
    fn msg3_key_data_is_wrapped_multiple_of_8() {
        let rsn = [0x30, 0x14, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 4, 1, 0, 0, 0x0f, 0xac, 2, 0, 0];
        let d = msg3_key_data(&[0u8; 16], &rsn, &[0u8; 16], 1).unwrap();
        assert_eq!(d.len() % 8, 0);
        assert_eq!(d.len(), 48 + 8);
    }

    #[test]
    fn reads_quoted_passphrase() {
        let conf = "network={\n\tssid=\"IEEE\"\n\tpsk=\"password\"\n}\n";
        assert_eq!(pmk_from_conf(conf, "IEEE").unwrap(), pmk("password", "IEEE"));
    }

    #[cfg(windows)]
    #[test]
    fn reads_a_sealed_one() {
        let sealed = Psk::Passphrase("password".into()).seal().unwrap();
        let conf = format!("network={{\n\tssid=\"IEEE\"\n\tpsk={sealed}\n}}\n");
        assert_eq!(pmk_from_conf(&conf, "IEEE").unwrap(), pmk("password", "IEEE"));
    }
}
