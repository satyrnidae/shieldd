//! WPS push-button registrar, as an AP w/ an internal registrar does it (hostapd's
//! wps_registrar.c), for one enrollee at a time.
//!
//! The enrollee associates open w/ a WPS IE, EAP-WSC carries M1-M8 over EAPOL, and M8 hands
//! over the credential. Push-button means the device password is "00000000". Everything here
//! is protocol only; bss.rs moves the frames.

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result, bail, ensure};
use hmac::{Hmac, Mac as _};
use num_bigint::BigUint;
use rand::RngCore;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

//region Attributes

pub mod attr {
    pub const AP_SETUP_LOCKED: u16 = 0x1057;
    pub const ASSOC_STATE: u16 = 0x1002;
    pub const AUTH_TYPE: u16 = 0x1003;
    pub const AUTH_TYPE_FLAGS: u16 = 0x1004;
    pub const AUTHENTICATOR: u16 = 0x1005;
    pub const CONFIG_METHODS: u16 = 0x1008;
    pub const CONFIG_ERROR: u16 = 0x1009;
    pub const CONN_TYPE_FLAGS: u16 = 0x100d;
    pub const CREDENTIAL: u16 = 0x100e;
    pub const ENCR_TYPE: u16 = 0x100f;
    pub const ENCR_TYPE_FLAGS: u16 = 0x1010;
    pub const DEVICE_NAME: u16 = 0x1011;
    pub const DEVICE_PASSWORD_ID: u16 = 0x1012;
    pub const E_HASH1: u16 = 0x1014;
    pub const E_HASH2: u16 = 0x1015;
    pub const E_SNONCE1: u16 = 0x1016;
    pub const E_SNONCE2: u16 = 0x1017;
    pub const ENCR_SETTINGS: u16 = 0x1018;
    pub const ENROLLEE_NONCE: u16 = 0x101a;
    pub const KEY_WRAP_AUTH: u16 = 0x101e;
    pub const MAC_ADDR: u16 = 0x1020;
    pub const MANUFACTURER: u16 = 0x1021;
    pub const MSG_TYPE: u16 = 0x1022;
    pub const MODEL_NAME: u16 = 0x1023;
    pub const MODEL_NUMBER: u16 = 0x1024;
    pub const NETWORK_INDEX: u16 = 0x1026;
    pub const NETWORK_KEY: u16 = 0x1027;
    pub const OS_VERSION: u16 = 0x102d;
    pub const PUBLIC_KEY: u16 = 0x1032;
    pub const REGISTRAR_NONCE: u16 = 0x1039;
    pub const RESPONSE_TYPE: u16 = 0x103b;
    pub const RF_BANDS: u16 = 0x103c;
    pub const R_HASH1: u16 = 0x103d;
    pub const R_HASH2: u16 = 0x103e;
    pub const R_SNONCE1: u16 = 0x103f;
    pub const R_SNONCE2: u16 = 0x1040;
    pub const SELECTED_REGISTRAR: u16 = 0x1041;
    pub const SERIAL_NUMBER: u16 = 0x1042;
    pub const WPS_STATE: u16 = 0x1044;
    pub const SSID: u16 = 0x1045;
    pub const UUID_E: u16 = 0x1047;
    pub const UUID_R: u16 = 0x1048;
    pub const VENDOR_EXT: u16 = 0x1049;
    pub const VERSION: u16 = 0x104a;
    pub const SEL_REG_CONFIG_METHODS: u16 = 0x1053;
    pub const PRIMARY_DEV_TYPE: u16 = 0x1054;
}

/// Message Type values
mod msg {
    pub const M1: u8 = 0x04;
    pub const M2: u8 = 0x05;
    pub const M3: u8 = 0x07;
    pub const M4: u8 = 0x08;
    pub const M5: u8 = 0x09;
    pub const M6: u8 = 0x0a;
    pub const M7: u8 = 0x0b;
    pub const M8: u8 = 0x0c;
    pub const NACK: u8 = 0x0e;
}

/// Version 1.0 in the Version attribute; 2.0 goes in the WFA vendor extension
const VERSION: u8 = 0x10;
const VERSION2: u8 = 0x20;
const WFA_OUI: [u8; 3] = [0x00, 0x37, 0x2a];
const WPS_OUI_TYPE: [u8; 4] = [0x00, 0x50, 0xf2, 0x04];

const AUTH_WPA2PSK: u16 = 0x0020;
const ENCR_AES: u16 = 0x0008;
const CONFIG_PBC: u16 = 0x0080;
const PW_ID_PUSH_BUTTON: u16 = 0x0004;
const WPS_STATE_CONFIGURED: u8 = 0x02;
const RESPONSE_AP: u8 = 0x03;
const RF_24GHZ: u8 = 0x01;
const CONN_ESS: u8 = 0x01;
/// Computer / PC, WFA OUI
const PRIMARY_DEV_TYPE: [u8; 8] = [0x00, 0x01, 0x00, 0x50, 0xf2, 0x04, 0x00, 0x01];

/// The push-button device password
const PBC_PASSWORD: &[u8] = b"00000000";

/// Attribute list builder
#[derive(Default)]
struct Tlv(Vec<u8>);

impl Tlv {
    fn put(&mut self, t: u16, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&t.to_be_bytes());
        self.0.extend_from_slice(&(v.len() as u16).to_be_bytes());
        self.0.extend_from_slice(v);
        self
    }

    fn u8(&mut self, t: u16, v: u8) -> &mut Self {
        self.put(t, &[v])
    }

    fn u16(&mut self, t: u16, v: u16) -> &mut Self {
        self.put(t, &v.to_be_bytes())
    }

    /// WFA vendor extension: Version2, and for PBC in beacons any MAC may enroll
    fn wfa_ext(&mut self, authorized_macs: bool) -> &mut Self {
        let mut v = WFA_OUI.to_vec();
        v.extend_from_slice(&[0x00, 1, VERSION2]);
        if authorized_macs {
            v.extend_from_slice(&[0x01, 6]);
            v.extend_from_slice(&[0xff; 6]);
        }
        self.put(attr::VENDOR_EXT, &v)
    }
}

/// Attributes of a message, in order
pub fn parse(msg: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < msg.len() {
        let h = msg.get(i..i + 4).context("truncated attribute header")?;
        let t = u16::from_be_bytes([h[0], h[1]]);
        let n = u16::from_be_bytes([h[2], h[3]]) as usize;
        let v = msg.get(i + 4..i + 4 + n).with_context(|| format!("attribute {t:#06x} runs past the end"))?;
        out.push((t, v));
        i += 4 + n;
    }
    Ok(out)
}

fn get<'a>(attrs: &[(u16, &'a [u8])], t: u16) -> Result<&'a [u8]> {
    attrs.iter().find(|(k, _)| *k == t).map(|(_, v)| *v).with_context(|| format!("no attribute {t:#06x}"))
}

fn get_n<const N: usize>(attrs: &[(u16, &[u8])], t: u16) -> Result<[u8; N]> {
    get(attrs, t)?.try_into().with_context(|| format!("attribute {t:#06x} isn't {N} bytes"))
}

fn text(attrs: &[(u16, &[u8])], t: u16) -> String {
    get(attrs, t).map(|v| String::from_utf8_lossy(v).into_owned()).unwrap_or_default()
}

//endregion

//region Information elements

/// Who we are to the enrollee. Plainly ourselves, not NVIDIA.
#[derive(Clone, Debug)]
pub struct Device {
    pub uuid: [u8; 16],
    pub name: String,
}

impl Device {
    /// Stable UUID from anything identifying the network, version 5 style like hostapd's
    /// uuid_gen_mac_addr
    pub fn from_seed(seed: &[u8], name: &str) -> Self {
        let h = Sha256::new().chain_update(b"shieldd wps uuid").chain_update(seed).finalize();
        let mut uuid: [u8; 16] = h[..16].try_into().unwrap();
        uuid[6] = (uuid[6] & 0x0f) | 0x50;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        Self { uuid, name: name.to_owned() }
    }

    fn put_identity(&self, t: &mut Tlv) {
        t.put(attr::MANUFACTURER, b"shieldd")
            .put(attr::MODEL_NAME, b"shieldd")
            .put(attr::MODEL_NUMBER, b"1")
            .put(attr::SERIAL_NUMBER, b"0")
            .put(attr::PRIMARY_DEV_TYPE, &PRIMARY_DEV_TYPE)
            .put(attr::DEVICE_NAME, self.name.as_bytes());
    }
}

/// The network the enrollee gets in M8
#[derive(Clone)]
pub struct Credential {
    pub ssid: String,
    pub passphrase: String,
}

fn vendor_ie(attrs: Tlv) -> Vec<u8> {
    let body = attrs.0;
    assert!(body.len() + 4 <= 255, "WPS IE needs splitting");
    let mut ie = vec![221, (body.len() + 4) as u8];
    ie.extend_from_slice(&WPS_OUI_TYPE);
    ie.extend_from_slice(&body);
    ie
}

/// Beacon WPS IE w/ push-button active
pub fn beacon_ie() -> Vec<u8> {
    let mut t = Tlv::default();
    t.u8(attr::VERSION, VERSION)
        .u8(attr::WPS_STATE, WPS_STATE_CONFIGURED)
        .u8(attr::SELECTED_REGISTRAR, 1)
        .u16(attr::DEVICE_PASSWORD_ID, PW_ID_PUSH_BUTTON)
        .u16(attr::SEL_REG_CONFIG_METHODS, CONFIG_PBC)
        .wfa_ext(true);
    vendor_ie(t)
}

/// Probe response WPS IE w/ push-button active
pub fn probe_resp_ie(dev: &Device) -> Vec<u8> {
    let mut t = Tlv::default();
    t.u8(attr::VERSION, VERSION)
        .u8(attr::WPS_STATE, WPS_STATE_CONFIGURED)
        .u8(attr::SELECTED_REGISTRAR, 1)
        .u16(attr::DEVICE_PASSWORD_ID, PW_ID_PUSH_BUTTON)
        .u16(attr::SEL_REG_CONFIG_METHODS, CONFIG_PBC)
        .u8(attr::RESPONSE_TYPE, RESPONSE_AP)
        .put(attr::UUID_E, &dev.uuid);
    dev.put_identity(&mut t);
    t.u16(attr::CONFIG_METHODS, CONFIG_PBC).u8(attr::RF_BANDS, RF_24GHZ).wfa_ext(true);
    vendor_ie(t)
}

/// Association response WPS IE, for an enrollee that associated for WPS
pub fn assoc_resp_ie() -> Vec<u8> {
    let mut t = Tlv::default();
    t.u8(attr::VERSION, VERSION).u8(attr::RESPONSE_TYPE, RESPONSE_AP).wfa_ext(false);
    vendor_ie(t)
}

/// WPS IE among the elements of a management frame body
pub fn has_ie(ies: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 <= ies.len() {
        let (id, len) = (ies[i], ies[i + 1] as usize);
        if id == 221 && ies.get(i + 2..i + 6) == Some(&WPS_OUI_TYPE[..]) {
            return true;
        }
        i += 2 + len;
    }
    false
}

//endregion

//region Crypto

/// RFC 3526 1536-bit MODP group, g = 2
const DH_PRIME: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA237327FFFFFFFFFFFFFFFF",
);
const DH_LEN: usize = 192;

fn prime() -> BigUint {
    BigUint::parse_bytes(DH_PRIME.as_bytes(), 16).expect("valid hex")
}

fn pad_dh(x: &BigUint) -> [u8; DH_LEN] {
    let b = x.to_bytes_be();
    let mut out = [0u8; DH_LEN];
    out[DH_LEN - b.len()..].copy_from_slice(&b);
    out
}

/// Private exponent and public key
fn dh_keypair() -> (BigUint, [u8; DH_LEN]) {
    let mut x = [0u8; DH_LEN];
    rand::thread_rng().fill_bytes(&mut x);
    let x = BigUint::from_bytes_be(&x);
    let public = BigUint::from(2u32).modpow(&x, &prime());
    (x, pad_dh(&public))
}

/// DHKey = SHA-256 of the shared secret, zero-padded to the group size
fn dh_key(private: &BigUint, peer_public: &[u8]) -> Result<[u8; 32]> {
    let p = prime();
    let peer = BigUint::from_bytes_be(peer_public);
    ensure!(peer > BigUint::from(1u32) && peer < &p - 1u32, "peer public key out of range");
    Ok(Sha256::digest(pad_dh(&peer.modpow(private, &p))).into())
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

pub struct Keys {
    pub auth: [u8; 32],
    pub wrap: [u8; 16],
}

/// KDK = HMAC(DHKey, N1 | enrollee MAC | N2), then the WPS KDF for 640 bits:
/// AuthKey 256, KeyWrapKey 128, EMSK 256 (unused)
fn derive_keys(dhkey: &[u8; 32], n1: &[u8; 16], mac: &[u8; 6], n2: &[u8; 16]) -> Keys {
    let kdk = hmac(dhkey, &[n1, mac, n2]);
    let mut out = Vec::with_capacity(96);
    for i in 1u32..=3 {
        out.extend_from_slice(&hmac(
            &kdk,
            &[&i.to_be_bytes(), b"Wi-Fi Easy and Secure Key Derivation", &640u32.to_be_bytes()],
        ));
    }
    Keys { auth: out[..32].try_into().unwrap(), wrap: out[32..48].try_into().unwrap() }
}

/// PSK1/PSK2: first 128 bits of HMAC(AuthKey, each half of the device password)
fn psks(auth: &[u8; 32], password: &[u8]) -> ([u8; 16], [u8; 16]) {
    let half = password.len().div_ceil(2);
    let p1 = hmac(auth, &[&password[..half]]);
    let p2 = hmac(auth, &[&password[half..]]);
    (p1[..16].try_into().unwrap(), p2[..16].try_into().unwrap())
}

/// Authenticator: first 64 bits of HMAC(AuthKey, previous message | this one w/o the attribute)
fn authenticator(auth: &[u8; 32], prev: &[u8], cur: &[u8]) -> [u8; 8] {
    hmac(auth, &[prev, cur])[..8].try_into().unwrap()
}

fn add_authenticator(auth: &[u8; 32], prev: &[u8], mut t: Tlv) -> Vec<u8> {
    let a = authenticator(auth, prev, &t.0);
    t.put(attr::AUTHENTICATOR, &a);
    t.0
}

/// The Authenticator has to be the last attribute
fn check_authenticator(auth: &[u8; 32], prev: &[u8], msg: &[u8]) -> Result<()> {
    ensure!(msg.len() >= 12, "message too short for an authenticator");
    let (body, tail) = msg.split_at(msg.len() - 12);
    ensure!(tail[..4] == [0x10, 0x05, 0x00, 0x08], "authenticator isn't the last attribute");
    ensure!(authenticator(auth, prev, body) == tail[4..], "authenticator mismatch");
    Ok(())
}

/// Encrypted Settings: IV | AES-128-CBC(KeyWrapKey, attrs | Key Wrap Authenticator), PKCS#7 padded
fn encrypt_settings(keys: &Keys, attrs: Tlv) -> Vec<u8> {
    let mut plain = attrs;
    let kwa = hmac(&keys.auth, &[&plain.0]);
    plain.put(attr::KEY_WRAP_AUTH, &kwa[..8]);
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let mut out = iv.to_vec();
    out.extend(Aes128CbcEnc::new(&keys.wrap.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain.0));
    out
}

/// Decrypts and checks the Key Wrap Authenticator; returns the attributes before it
fn decrypt_settings(keys: &Keys, data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() >= 32 && data.len().is_multiple_of(16), "encrypted settings of {} bytes", data.len());
    let (iv, ct) = data.split_at(16);
    let iv: [u8; 16] = iv.try_into().unwrap();
    let plain = Aes128CbcDec::new(&keys.wrap.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|_| anyhow::anyhow!("encrypted settings: bad padding"))?;
    ensure!(plain.len() >= 12, "encrypted settings too short");
    let (body, tail) = plain.split_at(plain.len() - 12);
    ensure!(tail[..4] == [0x10, 0x1e, 0x00, 0x08], "key wrap authenticator isn't last");
    ensure!(hmac(&keys.auth, &[body])[..8] == tail[4..], "key wrap authenticator mismatch");
    Ok(body.to_vec())
}

//endregion

//region Registrar

/// Known once M1 is in
struct Session {
    n1: [u8; 16],
    pke: [u8; DH_LEN],
    keys: Keys,
    psk1: [u8; 16],
    psk2: [u8; 16],
    r_s1: [u8; 16],
    r_s2: [u8; 16],
    e_hash1: [u8; 32],
    e_hash2: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    M1,
    M3,
    M5,
    M7,
    Done,
}

/// What the enrollee said about itself in M1
#[derive(Debug, Default, Clone)]
pub struct Enrollee {
    pub manufacturer: String,
    pub model: String,
    pub device_name: String,
}

pub struct Registrar {
    dev: Device,
    cred: Credential,
    mac: [u8; 6],
    private: BigUint,
    pkr: [u8; DH_LEN],
    n2: [u8; 16],
    step: Step,
    s: Option<Session>,
    /// Last message either way, for the next Authenticator
    last: Vec<u8>,
    pub enrollee: Enrollee,
}

impl Registrar {
    pub fn new(dev: Device, cred: Credential, enrollee_mac: [u8; 6]) -> Self {
        let (private, pkr) = dh_keypair();
        let mut n2 = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut n2);
        Self {
            dev,
            cred,
            mac: enrollee_mac,
            private,
            pkr,
            n2,
            step: Step::M1,
            s: None,
            last: Vec::new(),
            enrollee: Enrollee::default(),
        }
    }

    pub fn done(&self) -> bool {
        self.step == Step::Done
    }

    /// Takes M1, M3, M5 or M7 and returns M2, M4, M6 or M8
    pub fn on_message(&mut self, m: &[u8]) -> Result<Vec<u8>> {
        let attrs = parse(m)?;
        let ty = get_n::<1>(&attrs, attr::MSG_TYPE)?[0];
        let want = match self.step {
            Step::M1 => msg::M1,
            Step::M3 => msg::M3,
            Step::M5 => msg::M5,
            Step::M7 => msg::M7,
            Step::Done => bail!("message {ty:#04x} after M8"),
        };
        ensure!(ty == want, "got message type {ty:#04x}, expected {want:#04x}");
        if self.step != Step::M1 {
            ensure!(get(&attrs, attr::REGISTRAR_NONCE)? == self.n2, "registrar nonce mismatch");
            let s = self.s.as_ref().expect("session after M1");
            check_authenticator(&s.keys.auth, &self.last, m)?;
        }
        // Our reply's Authenticator covers this message
        self.last = m.to_vec();
        let reply = match self.step {
            Step::M1 => self.m2(m, &attrs)?,
            Step::M3 => self.m4(&attrs)?,
            Step::M5 => self.m6(&attrs)?,
            Step::M7 => self.m8(&attrs)?,
            Step::Done => unreachable!(),
        };
        self.step = match self.step {
            Step::M1 => Step::M3,
            Step::M3 => Step::M5,
            Step::M5 => Step::M7,
            _ => Step::Done,
        };
        self.last = reply.clone();
        Ok(reply)
    }

    /// WSC_NACK for the enrollee, once the nonces are known
    pub fn nack(&self, config_error: u16) -> Option<Vec<u8>> {
        let s = self.s.as_ref()?;
        let mut t = Tlv::default();
        t.u8(attr::VERSION, VERSION)
            .u8(attr::MSG_TYPE, msg::NACK)
            .put(attr::ENROLLEE_NONCE, &s.n1)
            .put(attr::REGISTRAR_NONCE, &self.n2)
            .u16(attr::CONFIG_ERROR, config_error)
            .wfa_ext(false);
        Some(t.0)
    }

    fn m2(&mut self, m1: &[u8], a: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        let mac: [u8; 6] = get_n(a, attr::MAC_ADDR)?;
        if mac != self.mac {
            tracing::debug!("M1 MAC differs from the transmitter; using M1's");
            self.mac = mac;
        }
        let pw_id = u16::from_be_bytes(get_n(a, attr::DEVICE_PASSWORD_ID)?);
        ensure!(pw_id == PW_ID_PUSH_BUTTON, "enrollee wants password id {pw_id:#06x}, not push-button");
        self.enrollee = Enrollee {
            manufacturer: text(a, attr::MANUFACTURER),
            model: text(a, attr::MODEL_NAME),
            device_name: text(a, attr::DEVICE_NAME),
        };
        let n1: [u8; 16] = get_n(a, attr::ENROLLEE_NONCE)?;
        let pke: [u8; DH_LEN] = get_n(a, attr::PUBLIC_KEY)?;
        let dhkey = dh_key(&self.private, &pke)?;
        let keys = derive_keys(&dhkey, &n1, &self.mac, &self.n2);
        let (psk1, psk2) = psks(&keys.auth, PBC_PASSWORD);
        let mut r_s1 = [0u8; 16];
        let mut r_s2 = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut r_s1);
        rand::thread_rng().fill_bytes(&mut r_s2);

        let mut t = Tlv::default();
        t.u8(attr::VERSION, VERSION)
            .u8(attr::MSG_TYPE, msg::M2)
            .put(attr::ENROLLEE_NONCE, &n1)
            .put(attr::REGISTRAR_NONCE, &self.n2)
            .put(attr::UUID_R, &self.dev.uuid)
            .put(attr::PUBLIC_KEY, &self.pkr)
            .u16(attr::AUTH_TYPE_FLAGS, AUTH_WPA2PSK)
            .u16(attr::ENCR_TYPE_FLAGS, ENCR_AES)
            .u8(attr::CONN_TYPE_FLAGS, CONN_ESS)
            .u16(attr::CONFIG_METHODS, CONFIG_PBC);
        self.dev.put_identity(&mut t);
        t.u8(attr::RF_BANDS, RF_24GHZ)
            .u16(attr::ASSOC_STATE, 0)
            .u16(attr::CONFIG_ERROR, 0)
            .u16(attr::DEVICE_PASSWORD_ID, PW_ID_PUSH_BUTTON)
            .put(attr::OS_VERSION, &0x8000_0000u32.to_be_bytes())
            .wfa_ext(false);
        let m2 = add_authenticator(&keys.auth, m1, t);
        self.s = Some(Session { n1, pke, keys, psk1, psk2, r_s1, r_s2, e_hash1: [0; 32], e_hash2: [0; 32] });
        Ok(m2)
    }

    fn m4(&mut self, a: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        let pkr = self.pkr;
        let s = self.s.as_mut().expect("session after M1");
        s.e_hash1 = get_n(a, attr::E_HASH1)?;
        s.e_hash2 = get_n(a, attr::E_HASH2)?;
        let r_hash1 = hmac(&s.keys.auth, &[&s.r_s1, &s.psk1, &s.pke, &pkr]);
        let r_hash2 = hmac(&s.keys.auth, &[&s.r_s2, &s.psk2, &s.pke, &pkr]);
        let mut enc = Tlv::default();
        enc.put(attr::R_SNONCE1, &s.r_s1);
        let mut t = Tlv::default();
        t.u8(attr::VERSION, VERSION)
            .u8(attr::MSG_TYPE, msg::M4)
            .put(attr::ENROLLEE_NONCE, &s.n1)
            .put(attr::R_HASH1, &r_hash1)
            .put(attr::R_HASH2, &r_hash2)
            .put(attr::ENCR_SETTINGS, &encrypt_settings(&s.keys, enc))
            .wfa_ext(false);
        Ok(add_authenticator(&s.keys.auth, &self.last, t))
    }

    /// E-Hash1 or E-Hash2 against the E-S1 / E-S2 the enrollee revealed
    fn check_e_hash(&self, a: &[(u16, &[u8])], second: bool) -> Result<()> {
        let s = self.s.as_ref().expect("session after M1");
        let plain = decrypt_settings(&s.keys, get(a, attr::ENCR_SETTINGS)?)?;
        let inner = parse(&plain)?;
        let (nonce_attr, psk, want) =
            if second { (attr::E_SNONCE2, &s.psk2, &s.e_hash2) } else { (attr::E_SNONCE1, &s.psk1, &s.e_hash1) };
        let e_s: [u8; 16] = get_n(&inner, nonce_attr)?;
        let got = hmac(&s.keys.auth, &[&e_s, psk, &s.pke, &self.pkr]);
        ensure!(&got == want, "E-Hash{} mismatch: enrollee used a different device password", 1 + second as u8);
        Ok(())
    }

    fn m6(&mut self, a: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        self.check_e_hash(a, false)?;
        let s = self.s.as_ref().expect("session after M1");
        let mut enc = Tlv::default();
        enc.put(attr::R_SNONCE2, &s.r_s2);
        let mut t = Tlv::default();
        t.u8(attr::VERSION, VERSION)
            .u8(attr::MSG_TYPE, msg::M6)
            .put(attr::ENROLLEE_NONCE, &s.n1)
            .put(attr::ENCR_SETTINGS, &encrypt_settings(&s.keys, enc))
            .wfa_ext(false);
        Ok(add_authenticator(&s.keys.auth, &self.last, t))
    }

    fn m8(&mut self, a: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        self.check_e_hash(a, true)?;
        let s = self.s.as_ref().expect("session after M1");
        let mut cred = Tlv::default();
        cred.u8(attr::NETWORK_INDEX, 1)
            .put(attr::SSID, self.cred.ssid.as_bytes())
            .u16(attr::AUTH_TYPE, AUTH_WPA2PSK)
            .u16(attr::ENCR_TYPE, ENCR_AES)
            .put(attr::NETWORK_KEY, self.cred.passphrase.as_bytes())
            .put(attr::MAC_ADDR, &self.mac);
        let mut enc = Tlv::default();
        enc.put(attr::CREDENTIAL, &cred.0);
        let mut t = Tlv::default();
        t.u8(attr::VERSION, VERSION)
            .u8(attr::MSG_TYPE, msg::M8)
            .put(attr::ENROLLEE_NONCE, &s.n1)
            .put(attr::ENCR_SETTINGS, &encrypt_settings(&s.keys, enc))
            .wfa_ext(false);
        Ok(add_authenticator(&s.keys.auth, &self.last, t))
    }
}

//endregion

//region EAP-WSC

pub const EAP_REQUEST: u8 = 1;
pub const EAP_RESPONSE: u8 = 2;
pub const EAP_FAILURE: u8 = 4;
const EAP_TYPE_IDENTITY: u8 = 1;
const EAP_TYPE_EXPANDED: u8 = 254;
const WSC_VENDOR_TYPE: u32 = 1;
const ENROLLEE_IDENTITY: &[u8] = b"WFA-SimpleConfig-Enrollee-1-0";

mod op {
    pub const START: u8 = 0x01;
    pub const ACK: u8 = 0x02;
    pub const NACK: u8 = 0x03;
    pub const MSG: u8 = 0x04;
    pub const DONE: u8 = 0x05;
    pub const FRAG_ACK: u8 = 0x06;
}
const FLAG_MF: u8 = 0x01;
const FLAG_LF: u8 = 0x02;

/// What the authenticator does next
#[derive(Debug)]
pub enum Action {
    /// EAP packet to send, replacing the last one for retransmission
    Send(Vec<u8>),
    /// Credential delivered; send EAP-Failure and let the enrollee go
    Delivered,
    /// Give up; send EAP-Failure
    Fail(String),
    /// Nothing, e.g. a duplicate response
    Ignore,
}

/// The authenticator's side of EAP-WSC around a Registrar
pub struct Eap {
    pub reg: Registrar,
    id: u8,
    /// Request that's out, for retransmission
    pub last: Vec<u8>,
    frags: Vec<u8>,
}

fn eap_packet(code: u8, id: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![code, id];
    p.extend_from_slice(&((4 + body.len()) as u16).to_be_bytes());
    p.extend_from_slice(body);
    p
}

fn wsc_request(id: u8, opcode: u8, data: &[u8]) -> Vec<u8> {
    let mut b = vec![EAP_TYPE_EXPANDED];
    b.extend_from_slice(&WFA_OUI);
    b.extend_from_slice(&WSC_VENDOR_TYPE.to_be_bytes());
    b.push(opcode);
    b.push(0);
    b.extend_from_slice(data);
    eap_packet(EAP_REQUEST, id, &b)
}

pub fn eap_failure(id: u8) -> Vec<u8> {
    eap_packet(EAP_FAILURE, id, &[])
}

impl Eap {
    /// Starts w/ EAP-Request/Identity
    pub fn new(reg: Registrar) -> Self {
        let last = eap_packet(EAP_REQUEST, 0, &[EAP_TYPE_IDENTITY]);
        Self { reg, id: 0, last, frags: Vec::new() }
    }

    pub fn id(&self) -> u8 {
        self.id
    }

    /// EAPOL-Start: begin again from the identity request
    pub fn restart(&mut self) -> Vec<u8> {
        self.id = self.id.wrapping_add(1);
        self.frags.clear();
        self.last = eap_packet(EAP_REQUEST, self.id, &[EAP_TYPE_IDENTITY]);
        self.last.clone()
    }

    fn request(&mut self, opcode: u8, data: &[u8]) -> Action {
        self.id = self.id.wrapping_add(1);
        self.last = wsc_request(self.id, opcode, data);
        Action::Send(self.last.clone())
    }

    /// One EAP packet from the enrollee
    pub fn on_packet(&mut self, p: &[u8]) -> Action {
        match self.handle(p) {
            Ok(a) => a,
            Err(e) => Action::Fail(format!("{e:#}")),
        }
    }

    fn handle(&mut self, p: &[u8]) -> Result<Action> {
        ensure!(p.len() >= 5, "short EAP packet");
        let (code, id) = (p[0], p[1]);
        let len = u16::from_be_bytes([p[2], p[3]]) as usize;
        let p = p.get(..len).context("EAP length past the frame")?;
        if code != EAP_RESPONSE || id != self.id {
            return Ok(Action::Ignore);
        }
        match p[4] {
            EAP_TYPE_IDENTITY => {
                let identity = &p[5..];
                ensure!(
                    identity == ENROLLEE_IDENTITY,
                    "identity {:?} isn't a WPS enrollee",
                    String::from_utf8_lossy(identity)
                );
                Ok(self.request(op::START, &[]))
            }
            EAP_TYPE_EXPANDED => {
                ensure!(p.len() >= 14, "short EAP-WSC packet");
                ensure!(p[5..8] == WFA_OUI && p[8..12] == WSC_VENDOR_TYPE.to_be_bytes(), "not EAP-WSC");
                let (opcode, flags) = (p[12], p[13]);
                let data = if flags & FLAG_LF != 0 { p.get(16..).context("short LF")? } else { &p[14..] };
                match opcode {
                    op::MSG if flags & FLAG_MF != 0 => {
                        self.frags.extend_from_slice(data);
                        Ok(self.request(op::FRAG_ACK, &[]))
                    }
                    op::MSG => {
                        let mut m = std::mem::take(&mut self.frags);
                        m.extend_from_slice(data);
                        match self.reg.on_message(&m) {
                            Ok(reply) => Ok(self.request(op::MSG, &reply)),
                            Err(e) => {
                                if let Some(nack) = self.reg.nack(0) {
                                    // The enrollee answers a NACK w/ its own, then we fail
                                    tracing::warn!("WPS: {e:#}; sending NACK");
                                    return Ok(self.request(op::NACK, &nack));
                                }
                                Err(e)
                            }
                        }
                    }
                    op::DONE if self.reg.done() => Ok(Action::Delivered),
                    op::DONE => bail!("WSC_Done before M8"),
                    op::NACK => bail!("enrollee sent WSC_NACK{}", nack_reason(data)),
                    op::ACK => Ok(Action::Ignore),
                    other => bail!("unexpected EAP-WSC opcode {other:#04x}"),
                }
            }
            other => bail!("EAP response type {other}, not WSC"),
        }
    }
}

fn nack_reason(data: &[u8]) -> String {
    parse(data)
        .ok()
        .and_then(|a| get_n::<2>(&a, attr::CONFIG_ERROR).ok())
        .map(|e| format!(", config error {}", u16::from_be_bytes(e)))
        .unwrap_or_default()
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    const ENROLLEE_MAC: [u8; 6] = [0x00, 0x04, 0x4b, 0x21, 0x92, 0x06];

    fn cred() -> Credential {
        Credential { ssid: "DIRECT-FC-organized-baboon".into(), passphrase: "s3cretPW".into() }
    }

    #[test]
    fn prime_is_rfc3526_group5() {
        let p = prime();
        assert_eq!(p.bits(), 1536);
        // Spot checks against the RFC text
        assert_eq!(&p.to_bytes_be()[8..12], &[0xc9, 0x0f, 0xda, 0xa2]);
        assert_eq!(&p.to_bytes_be()[180..184], &[0xca, 0x23, 0x73, 0x27]);
    }

    #[test]
    fn dh_agrees_both_ways() {
        let (a, pa) = dh_keypair();
        let (b, pb) = dh_keypair();
        assert_eq!(dh_key(&a, &pb).unwrap(), dh_key(&b, &pa).unwrap());
        assert!(dh_key(&a, &[1]).is_err());
    }

    #[test]
    fn pbc_psks_split_the_password_in_half() {
        let auth = [7u8; 32];
        let (p1, p2) = psks(&auth, PBC_PASSWORD);
        assert_eq!(p1, p2, "both halves are \"0000\"");
        assert_eq!(p1[..], hmac(&auth, &[b"0000"])[..16]);
    }

    #[test]
    fn settings_round_trip_and_reject_tampering() {
        let keys = Keys { auth: [1; 32], wrap: [2; 16] };
        let mut t = Tlv::default();
        t.put(attr::R_SNONCE1, &[9; 16]);
        let enc = encrypt_settings(&keys, t);
        assert_eq!(enc.len() % 16, 0);
        let plain = decrypt_settings(&keys, &enc).unwrap();
        assert_eq!(get(&parse(&plain).unwrap(), attr::R_SNONCE1).unwrap(), &[9; 16]);
        let mut bad = enc.clone();
        bad[20] ^= 1;
        assert!(decrypt_settings(&keys, &bad).is_err());
    }

    #[test]
    fn ies_are_well_formed() {
        let dev = Device::from_seed(b"DIRECT-FC-organized-baboon", "shieldd");
        for ie in [beacon_ie(), probe_resp_ie(&dev), assoc_resp_ie()] {
            assert_eq!(ie[0], 221);
            assert_eq!(ie[1] as usize, ie.len() - 2);
            assert!(has_ie(&ie));
            let a = parse(&ie[6..]).unwrap();
            assert_eq!(get(&a, attr::VERSION).unwrap(), &[VERSION]);
        }
        let b = beacon_ie();
        let a = parse(&b[6..]).unwrap();
        assert_eq!(get(&a, attr::SELECTED_REGISTRAR).unwrap(), &[1]);
        assert_eq!(get(&a, attr::DEVICE_PASSWORD_ID).unwrap(), &[0, 4]);
        assert_eq!(dev.uuid[6] >> 4, 5);
    }

    /// An enrollee written straight from the spec, independent of the registrar's message code
    struct TestEnrollee {
        private: BigUint,
        pke: [u8; DH_LEN],
        n1: [u8; 16],
        e_s1: [u8; 16],
        e_s2: [u8; 16],
        keys: Option<Keys>,
        pkr: Vec<u8>,
        n2: Vec<u8>,
        last: Vec<u8>,
        password: &'static [u8],
    }

    impl TestEnrollee {
        fn new(password: &'static [u8]) -> Self {
            let (private, pke) = dh_keypair();
            Self {
                private,
                pke,
                n1: [0x11; 16],
                e_s1: [0x21; 16],
                e_s2: [0x22; 16],
                keys: None,
                pkr: Vec::new(),
                n2: Vec::new(),
                last: Vec::new(),
                password,
            }
        }

        fn m1(&mut self) -> Vec<u8> {
            let mut t = Tlv::default();
            t.u8(attr::VERSION, 0x10)
                .u8(attr::MSG_TYPE, msg::M1)
                .put(attr::UUID_E, &[0x33; 16])
                .put(attr::MAC_ADDR, &ENROLLEE_MAC)
                .put(attr::ENROLLEE_NONCE, &self.n1)
                .put(attr::PUBLIC_KEY, &self.pke)
                .u16(attr::AUTH_TYPE_FLAGS, 0x0023)
                .u16(attr::ENCR_TYPE_FLAGS, 0x000d)
                .u8(attr::CONN_TYPE_FLAGS, 1)
                .u16(attr::CONFIG_METHODS, 0x0080)
                .u8(attr::WPS_STATE, 1)
                .put(attr::MANUFACTURER, b"Test")
                .put(attr::MODEL_NAME, b"Pad")
                .put(attr::DEVICE_NAME, b"Test Pad")
                .u16(attr::DEVICE_PASSWORD_ID, PW_ID_PUSH_BUTTON);
            self.last = t.0.clone();
            t.0
        }

        fn take(&mut self, m: &[u8]) -> Vec<(u16, Vec<u8>)> {
            let keys = self.keys.as_ref().unwrap();
            check_authenticator(&keys.auth, &self.last, m).unwrap();
            self.last = m.to_vec();
            parse(m).unwrap().into_iter().map(|(t, v)| (t, v.to_vec())).collect()
        }

        fn send(&mut self, t: Tlv) -> Vec<u8> {
            let m = add_authenticator(&self.keys.as_ref().unwrap().auth, &self.last, t);
            self.last = m.clone();
            m
        }

        fn m3(&mut self, m2: &[u8]) -> Vec<u8> {
            let a = parse(m2).unwrap();
            self.pkr = get(&a, attr::PUBLIC_KEY).unwrap().to_vec();
            self.n2 = get(&a, attr::REGISTRAR_NONCE).unwrap().to_vec();
            let dhkey = dh_key(&self.private, &self.pkr).unwrap();
            self.keys = Some(derive_keys(&dhkey, &self.n1, &ENROLLEE_MAC, &self.n2.clone().try_into().unwrap()));
            self.take(m2);
            let auth = self.keys.as_ref().unwrap().auth;
            let (psk1, psk2) = psks(&auth, self.password);
            let mut t = Tlv::default();
            t.u8(attr::VERSION, 0x10)
                .u8(attr::MSG_TYPE, msg::M3)
                .put(attr::REGISTRAR_NONCE, &self.n2)
                .put(attr::E_HASH1, &hmac(&auth, &[&self.e_s1, &psk1, &self.pke, &self.pkr]))
                .put(attr::E_HASH2, &hmac(&auth, &[&self.e_s2, &psk2, &self.pke, &self.pkr]));
            self.send(t)
        }

        fn encrypted_nonce(&mut self, m: &[u8], ty: u8, nonce_attr: u16, nonce: [u8; 16]) -> Vec<u8> {
            self.take(m);
            let mut enc = Tlv::default();
            enc.put(nonce_attr, &nonce);
            let es = encrypt_settings(self.keys.as_ref().unwrap(), enc);
            let mut t = Tlv::default();
            t.u8(attr::VERSION, 0x10)
                .u8(attr::MSG_TYPE, ty)
                .put(attr::REGISTRAR_NONCE, &self.n2)
                .put(attr::ENCR_SETTINGS, &es);
            self.send(t)
        }
    }

    fn registrar() -> Registrar {
        Registrar::new(Device::from_seed(b"DIRECT-FC-organized-baboon", "shieldd"), cred(), ENROLLEE_MAC)
    }

    #[test]
    fn full_exchange_delivers_the_credential() {
        let mut reg = registrar();
        let mut e = TestEnrollee::new(PBC_PASSWORD);
        let m1 = e.m1();
        let m2 = reg.on_message(&m1).unwrap();
        assert_eq!(reg.enrollee.device_name, "Test Pad");
        let m3 = e.m3(&m2);
        let m4 = reg.on_message(&m3).unwrap();

        // M4 proves the registrar knows the password: check R-Hash1 against R-S1
        let a4 = parse(&m4).unwrap();
        let find = |a: &[(u16, Vec<u8>)], t: u16| a.iter().find(|(k, _)| *k == t).unwrap().1.clone();
        let auth = e.keys.as_ref().unwrap().auth;
        let plain = decrypt_settings(e.keys.as_ref().unwrap(), get(&a4, attr::ENCR_SETTINGS).unwrap()).unwrap();
        let r_s1 = get(&parse(&plain).unwrap(), attr::R_SNONCE1).unwrap().to_vec();
        let (psk1, _) = psks(&auth, PBC_PASSWORD);
        assert_eq!(get(&a4, attr::R_HASH1).unwrap(), hmac(&auth, &[&r_s1, &psk1, &e.pke, &e.pkr]));

        let e_s1 = e.e_s1;
        let m5 = e.encrypted_nonce(&m4, msg::M5, attr::E_SNONCE1, e_s1);
        let m6 = reg.on_message(&m5).unwrap();
        let e_s2 = e.e_s2;
        let m7 = e.encrypted_nonce(&m6, msg::M7, attr::E_SNONCE2, e_s2);
        let m8 = reg.on_message(&m7).unwrap();
        assert!(reg.done());

        let a8 = e.take(&m8);
        let plain = decrypt_settings(e.keys.as_ref().unwrap(), &find(&a8, attr::ENCR_SETTINGS)).unwrap();
        let outer = parse(&plain).unwrap();
        let c = parse(get(&outer, attr::CREDENTIAL).unwrap()).unwrap();
        assert_eq!(get(&c, attr::SSID).unwrap(), b"DIRECT-FC-organized-baboon");
        assert_eq!(get(&c, attr::NETWORK_KEY).unwrap(), b"s3cretPW");
        assert_eq!(get(&c, attr::AUTH_TYPE).unwrap(), &[0x00, 0x20]);
        assert_eq!(get(&c, attr::ENCR_TYPE).unwrap(), &[0x00, 0x08]);
        assert_eq!(get(&c, attr::MAC_ADDR).unwrap(), &ENROLLEE_MAC);
    }

    #[test]
    fn wrong_password_is_caught_at_m5() {
        let mut reg = registrar();
        let mut e = TestEnrollee::new(b"12345670");
        let m2 = reg.on_message(&e.m1()).unwrap();
        let m3 = e.m3(&m2);
        let m4 = reg.on_message(&m3).unwrap();
        let e_s1 = e.e_s1;
        let m5 = e.encrypted_nonce(&m4, msg::M5, attr::E_SNONCE1, e_s1);
        let err = reg.on_message(&m5).unwrap_err().to_string();
        assert!(err.contains("E-Hash1"), "{err}");
        assert!(reg.nack(0).is_some());
    }

    #[test]
    fn tampered_m3_fails_the_authenticator() {
        let mut reg = registrar();
        let mut e = TestEnrollee::new(PBC_PASSWORD);
        let m2 = reg.on_message(&e.m1()).unwrap();
        let mut m3 = e.m3(&m2);
        let n = m3.len();
        m3[n - 20] ^= 1;
        assert!(reg.on_message(&m3).unwrap_err().to_string().contains("authenticator"));
    }

    fn wsc_response(id: u8, opcode: u8, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut b = vec![EAP_TYPE_EXPANDED];
        b.extend_from_slice(&WFA_OUI);
        b.extend_from_slice(&WSC_VENDOR_TYPE.to_be_bytes());
        b.push(opcode);
        b.push(flags);
        b.extend_from_slice(data);
        eap_packet(EAP_RESPONSE, id, &b)
    }

    #[test]
    fn eap_identity_then_start_then_fragmented_m1() {
        let mut eap = Eap::new(registrar());
        assert_eq!(eap.last, [1, 0, 0, 5, 1]);
        let mut ident = vec![EAP_TYPE_IDENTITY];
        ident.extend_from_slice(ENROLLEE_IDENTITY);
        let Action::Send(start) = eap.on_packet(&eap_packet(EAP_RESPONSE, 0, &ident)) else { panic!() };
        assert_eq!(start[1], 1);
        assert_eq!(&start[12..14], &[op::START, 0]);

        // A late duplicate of the identity response is ignored
        assert!(matches!(eap.on_packet(&eap_packet(EAP_RESPONSE, 0, &ident)), Action::Ignore));

        let mut e = TestEnrollee::new(PBC_PASSWORD);
        let m1 = e.m1();
        let (a, b) = m1.split_at(100);
        let mut first = (m1.len() as u16).to_be_bytes().to_vec();
        first.extend_from_slice(a);
        let Action::Send(ack) = eap.on_packet(&wsc_response(1, op::MSG, FLAG_MF | FLAG_LF, &first)) else { panic!() };
        assert_eq!(ack[12], op::FRAG_ACK);
        let Action::Send(m2) = eap.on_packet(&wsc_response(2, op::MSG, 0, b)) else { panic!() };
        assert_eq!(m2[12], op::MSG);
        let a = parse(&m2[14..]).unwrap();
        assert_eq!(get(&a, attr::MSG_TYPE).unwrap(), &[msg::M2]);
    }

    #[test]
    fn eap_rejects_a_non_wps_identity() {
        let mut eap = Eap::new(registrar());
        let mut ident = vec![EAP_TYPE_IDENTITY];
        ident.extend_from_slice(b"user@example");
        assert!(matches!(eap.on_packet(&eap_packet(EAP_RESPONSE, 0, &ident)), Action::Fail(_)));
    }
}
