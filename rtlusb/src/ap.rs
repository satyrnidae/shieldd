//! AP mode: beacon/probe response frames, port setup and the hardware beacon.
//!
//! What the controller needs: its paired group's SSID on the channel it paired on, OFDM rates
//! only, WPA2-PSK/CCMP and WMM. It ignores 1 Mbps CCK frames, so EAPOL sent at that rate never
//! gets an ACK. Rejoining, it sends plain directed probes, broadcast BSSID, w/ only SSID and
//! rates 6-24 Mbps, no P2P or WPS IEs.

use std::time::Duration;

use anyhow::{Result, bail};
use tracing::debug;

use crate::chip::Chip;
use crate::regs::*;
use crate::tx::{Tx, TxInfo};
use crate::wps;

pub struct ApConfig {
    pub ssid: String,
    pub bssid: [u8; 6],
    pub channel: u8,
    /// TU
    pub beacon_int: u16,
    /// Empty SSID in beacons, and only probes naming the SSID get an answer. The controller
    /// probes by name, so it still finds us; scanners don't. Ignored while pairing.
    pub hidden: bool,
    /// Pairing: WPS push-button active, and the credential an enrollee gets
    pub wps: Option<WpsConfig>,
}

#[derive(Clone)]
pub struct WpsConfig {
    pub device: wps::Device,
    pub credential: wps::Credential,
}

const CAP_ESS: u16 = 1 << 0;
const CAP_PRIVACY: u16 = 1 << 4;
const CAP_SHORT_PREAMBLE: u16 = 1 << 5;
const CAP_SHORT_SLOT: u16 = 1 << 10;

/// 6, 9, 12, 18, 24, 36, 48, 54 Mbps; 6/12/24 basic
const RATES: [u8; 8] = [0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c];

/// RSN: version 1, CCMP group, one CCMP pairwise, one PSK AKM, no capabilities
pub const RSN: [u8; 20] = [
    0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x02, 0x00,
    0x00,
];

/// WMM parameter element, hostapd's default AC parameters
pub const WMM_PARAM: [u8; 24] = [
    0x00, 0x50, 0xf2, 0x02, 0x01, 0x01, 0x00, 0x00, // OUI, type, subtype, version, QoS info, reserved
    0x03, 0xa4, 0x00, 0x00, // BE
    0x27, 0xa4, 0x00, 0x00, // BK
    0x42, 0x43, 0x5e, 0x00, // VI
    0x62, 0x32, 0x2f, 0x00, // VO
];

fn ie(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    out.push(body.len() as u8);
    out.extend_from_slice(body);
}

impl ApConfig {
    fn header(&self, subtype_fc: u16, da: &[u8; 6]) -> Vec<u8> {
        let mut f = Vec::with_capacity(256);
        f.extend_from_slice(&subtype_fc.to_le_bytes());
        f.extend_from_slice(&[0, 0]); // duration
        f.extend_from_slice(da);
        f.extend_from_slice(&self.bssid);
        f.extend_from_slice(&self.bssid);
        f.extend_from_slice(&[0, 0]); // sequence, filled by hardware
        f.extend_from_slice(&[0; 8]); // timestamp, filled by hardware
        f.extend_from_slice(&self.beacon_int.to_le_bytes());
        let cap = CAP_ESS | CAP_PRIVACY | CAP_SHORT_PREAMBLE | CAP_SHORT_SLOT;
        f.extend_from_slice(&cap.to_le_bytes());
        f
    }

    /// An enrollee has to find us by scanning, so pairing always shows the SSID
    fn hidden_now(&self) -> bool {
        self.hidden && self.wps.is_none()
    }

    fn body_ies(&self, f: &mut Vec<u8>, beacon: bool) {
        let ssid = if beacon && self.hidden_now() { &[][..] } else { self.ssid.as_bytes() };
        ie(f, 0, ssid);
        ie(f, 1, &RATES);
        ie(f, 3, &[self.channel]);
        if beacon {
            // DTIM count 0, period 1, no buffered traffic: we never buffer for dozing clients
            ie(f, 5, &[0, 1, 0, 0]);
        }
        ie(f, 48, &RSN);
        ie(f, 221, &WMM_PARAM);
        if let Some(w) = &self.wps {
            f.extend(if beacon { wps::beacon_ie() } else { wps::probe_resp_ie(&w.device) });
        }
    }

    pub fn beacon(&self) -> Vec<u8> {
        let mut f = self.header(0x0080, &[0xff; 6]);
        self.body_ies(&mut f, true);
        f
    }

    pub fn probe_resp(&self, da: &[u8; 6]) -> Vec<u8> {
        let mut f = self.header(0x0050, da);
        self.body_ies(&mut f, false);
        f
    }

    /// Probe request for our SSID, or a wildcard unless hidden
    pub fn wants(&self, probe_req: &[u8]) -> bool {
        match crate::rx::ssid(probe_req) {
            Some(s) => s == self.ssid || (s.is_empty() && !self.hidden_now()),
            None => false,
        }
    }
}

impl Chip {
    /// Port 0 as an AP, rtw_vif_port_config for NL80211_IFTYPE_AP
    pub fn start_ap(&self, tx: &mut Tx, cfg: &ApConfig) -> Result<()> {
        let rtl = &self.rtl;
        self.set_mac(&cfg.bssid)?;
        rtl.write(BSSID, &cfg.bssid)?;
        rtl.w32_mask(CR, 0x30000, NET_TYPE_AP)?;
        rtl.w16(BCN_INTERVAL, cfg.beacon_int)?;
        // rtw_set_dtim_period(1)
        rtl.set32(TCR, TCR_UPDATE_TIMIE)?;
        rtl.w8(DTIM_COUNTER_ROOT, 0)?;
        rtl.w8(BCN_CTRL, (EN_BCN_FUNCTION | DIS_TSF_UDT) as u8)?;
        self.download_beacon(tx, &cfg.beacon())?;
        rtl.set32(FWHW_TXQ_CTRL, EN_BCNQ_DL)
    }

    /// rtw_fw_write_data_rsvd_page for an 8051 chip: the beacon goes into the first reserved page
    pub fn download_beacon(&self, tx: &mut Tx, beacon: &[u8]) -> Result<()> {
        let rtl = &self.rtl;
        let bcn_ctrl = rtl.r8(BCN_CTRL)?;
        // Writing 1 clears BCN_VALID
        rtl.set32(DWBCN0_CTRL, BCN_VALID)?;
        let cr1 = rtl.r8(CR + 1)?;
        rtl.w8(CR + 1, cr1 | (ENSWBCN >> 8) as u8)?;
        rtl.w8(BCN_CTRL, (bcn_ctrl & !(EN_BCN_FUNCTION as u8)) | DIS_TSF_UDT as u8)?;

        let res = tx.send(rtl, TxInfo::beacon(), beacon).and_then(|()| {
            if rtl.poll32(DWBCN0_CTRL, BCN_VALID, 1, Duration::from_millis(20))? {
                Ok(())
            } else {
                bail!("beacon never became valid")
            }
        });

        rtl.w16(FIFOPAGE_CTRL_2, (crate::chip::RSVD_BOUNDARY as u16) | BCN_VALID_V1)?;
        rtl.w8(BCN_CTRL, bcn_ctrl)?;
        rtl.w8(CR + 1, cr1)?;
        debug!(len = beacon.len(), "beacon downloaded");
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ApConfig {
        ApConfig {
            ssid: "DIRECT-0S".into(),
            bssid: [0, 0xc0, 0xca, 0xbc, 0xe1, 0xce],
            channel: 1,
            beacon_int: 100,
            hidden: false,
            wps: None,
        }
    }

    #[test]
    fn pairing_shows_the_ssid_and_advertises_push_button() {
        let device = wps::Device::from_seed(b"x", "shieldd");
        let credential = wps::Credential { ssid: "DIRECT-0S".into(), passphrase: "p".into() };
        let c = ApConfig { hidden: true, wps: Some(WpsConfig { device, credential }), ..cfg() };
        let b = c.beacon();
        assert_eq!(&b[38..47], b"DIRECT-0S");
        assert!(wps::has_ie(&b[36..]));
        assert!(wps::has_ie(&c.probe_resp(&[0; 6])[36..]));
        assert!(!wps::has_ie(&cfg().beacon()[36..]));
    }

    #[test]
    fn hidden_blanks_the_beacon_and_ignores_wildcards() {
        let c = ApConfig { hidden: true, ..cfg() };
        let mut b = c.beacon();
        assert_eq!(&b[36..38], &[0, 0]);
        b.extend([0; 4]);
        assert_eq!(crate::rx::ssid(&b).as_deref(), Some(""));
        let mut r = c.probe_resp(&[0; 6]);
        r.extend([0; 4]);
        assert_eq!(crate::rx::ssid(&r).as_deref(), Some("DIRECT-0S"));
        // Wildcard probe: header, empty SSID, rates, FCS
        let mut wild = vec![0x40, 0, 0, 0];
        wild.extend([0xff; 6]);
        wild.extend([2; 6]);
        wild.extend([0xff; 6]);
        wild.extend([0, 0, 0, 0, 1, 1, 0x0c, 0, 0, 0, 0]);
        assert!(cfg().wants(&wild));
        assert!(!c.wants(&wild));
    }

    #[test]
    fn beacon_layout() {
        let b = cfg().beacon();
        assert_eq!(&b[0..2], &[0x80, 0x00]);
        assert_eq!(&b[4..10], &[0xff; 6]);
        assert_eq!(&b[10..16], &cfg().bssid);
        assert_eq!(u16::from_le_bytes([b[32], b[33]]), 100);
        assert_eq!(&b[36..38], &[0, 9]);
        assert_eq!(&b[38..47], b"DIRECT-0S");
        let mut fcs = b.clone();
        fcs.extend([0; 4]);
        assert_eq!(crate::rx::ssid(&fcs).as_deref(), Some("DIRECT-0S"));
    }

    #[test]
    fn answers_the_controllers_probe() {
        // The controller reconnecting on channel 1, as captured: its first frame, FCS included
        let probe = [
            0x40, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x04, 0x4b, 0x21, 0x92, 0x06, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0x60, 0x06, 0x00, 0x09, 0x44, 0x49, 0x52, 0x45, 0x43, 0x54, 0x2d, 0x30, 0x53, 0x01,
            0x05, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x08, 0xc7, 0xec, 0xe0,
        ];
        assert!(cfg().wants(&probe));
        let r = cfg().probe_resp(&[0x00, 0x04, 0x4b, 0x21, 0x92, 0x06]);
        assert_eq!(&r[0..2], &[0x50, 0x00]);
        assert_eq!(&r[4..10], &[0x00, 0x04, 0x4b, 0x21, 0x92, 0x06]);
    }
}
