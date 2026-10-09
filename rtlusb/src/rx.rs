//! RX descriptor parsing, as rtw88 rx.c and rtw88xxa_query_phy_status.

pub const DESC_SIZE: usize = 24;

/// One received frame inside a bulk IN transfer
#[allow(dead_code)] // Crypto and TSF fields are for M3
#[derive(Debug)]
pub struct RxPkt<'a> {
    pub crc_err: bool,
    pub icv_err: bool,
    pub decrypted: bool,
    pub c2h: bool,
    pub rate: u8,
    pub tsf_low: u32,
    /// Signal in dBm, when a PHY status was attached
    pub dbm: Option<i8>,
    /// 802.11 frame, FCS included
    pub frame: &'a [u8],
}

fn w(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i * 4..i * 4 + 4].try_into().unwrap())
}

/// rtw8821a_cck_rx_pwr
fn cck_dbm(lna: u8, vga: u8) -> i8 {
    const LNA_GAIN: [i8; 6] = [15, -1, -17, 0, -30, -38];
    match lna {
        0 | 1 | 2 | 4 | 5 => LNA_GAIN[lna as usize] - 2 * vga as i8,
        _ => 0,
    }
}

fn phy_dbm(rate: u8, phy: &[u8]) -> Option<i8> {
    if phy.len() < 8 {
        return None;
    }
    if rate <= 3 {
        let w1 = w(phy, 1);
        Some(cck_dbm(((w1 >> 13) & 0x7) as u8, ((w1 >> 8) & 0x1f) as u8))
    } else {
        Some(((w(phy, 0) & 0x7f) as i16 - 110).max(-120) as i8)
    }
}

/// Splits a bulk IN transfer into frames. Stops at the first malformed descriptor.
pub fn split(buf: &[u8]) -> Vec<RxPkt<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + DESC_SIZE <= buf.len() {
        let d = &buf[at..at + DESC_SIZE];
        let w0 = w(d, 0);
        let pkt_len = (w0 & 0x3fff) as usize;
        let drv_info = ((w0 >> 16) & 0xf) as usize * 8;
        let shift = ((w0 >> 24) & 0x3) as usize;
        let physt = w0 & (1 << 26) != 0;
        let swdec = w0 & (1 << 27) != 0;
        let enc = (w0 >> 20) & 0x7;
        let c2h = w(d, 2) & (1 << 28) != 0;
        let rate = (w(d, 3) & 0x7f) as u8;

        let body = at + DESC_SIZE + shift;
        let frame_at = body + drv_info;
        let end = frame_at + pkt_len;
        if end > buf.len() {
            break;
        }
        let dbm = if physt && !c2h { phy_dbm(rate, &buf[body..frame_at]) } else { None };
        out.push(RxPkt {
            crc_err: w0 & (1 << 14) != 0,
            icv_err: w0 & (1 << 15) != 0,
            decrypted: !swdec && enc != 0,
            c2h,
            rate,
            tsf_low: w(d, 5),
            dbm,
            frame: &buf[frame_at..end],
        });
        at += (end - at).next_multiple_of(8);
    }
    out
}

//region 802.11 summary

pub fn rate_name(rate: u8) -> String {
    const LEGACY: [&str; 12] = ["1M", "2M", "5.5M", "11M", "6M", "9M", "12M", "18M", "24M", "36M", "48M", "54M"];
    match rate {
        0..=11 => LEGACY[rate as usize].to_string(),
        12..=43 => format!("MCS{}", rate - 12),
        44.. => format!("VHT{}SS-MCS{}", (rate - 44) / 10 + 1, (rate - 44) % 10),
    }
}

pub fn frame_kind(fc: u16) -> &'static str {
    let ty = (fc >> 2) & 0x3;
    let sub = (fc >> 4) & 0xf;
    match (ty, sub) {
        (0, 0) => "assoc-req",
        (0, 1) => "assoc-resp",
        (0, 2) => "reassoc-req",
        (0, 3) => "reassoc-resp",
        (0, 4) => "probe-req",
        (0, 5) => "probe-resp",
        (0, 8) => "beacon",
        (0, 10) => "disassoc",
        (0, 11) => "auth",
        (0, 12) => "deauth",
        (0, 13) => "action",
        (0, _) => "mgmt",
        (1, 8) => "block-ack-req",
        (1, 9) => "block-ack",
        (1, 10) => "ps-poll",
        (1, 11) => "rts",
        (1, 12) => "cts",
        (1, 13) => "ack",
        (1, _) => "ctrl",
        (2, 0) => "data",
        (2, 4) => "null",
        (2, 8) => "qos-data",
        (2, 12) => "qos-null",
        (2, _) => "data",
        _ => "?",
    }
}

/// SSID element from a beacon, probe request or probe response body
pub fn ssid(frame: &[u8]) -> Option<String> {
    let fc = u16::from_le_bytes([frame[0], frame[1]]);
    let body = match frame_kind(fc) {
        "beacon" | "probe-resp" => 24 + 12,
        "probe-req" => 24,
        _ => return None,
    };
    let ies = frame.get(body..frame.len().saturating_sub(4))?;
    let mut i = 0;
    while i + 2 <= ies.len() {
        let (id, len) = (ies[i], ies[i + 1] as usize);
        let val = ies.get(i + 2..i + 2 + len)?;
        if id == 0 {
            return Some(String::from_utf8_lossy(val).into_owned());
        }
        i += 2 + len;
    }
    None
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(pkt_len: u32, drv_info_units: u32, physt: bool, rate: u32) -> Vec<u8> {
        let w0 = pkt_len | drv_info_units << 16 | (physt as u32) << 26;
        let mut d = vec![0; DESC_SIZE];
        d[0..4].copy_from_slice(&w0.to_le_bytes());
        d[12..16].copy_from_slice(&rate.to_le_bytes());
        d
    }

    #[test]
    fn splits_aggregated_transfer_on_8_byte_boundaries() {
        let mut buf = desc(14, 4, true, 4);
        let mut phy = vec![0; 32];
        phy[0] = 60; // gain A -> -50 dBm
        buf.extend(&phy);
        buf.extend([0xd4, 0x00, 0, 0, 1, 2, 3, 4, 5, 6, 0xaa, 0xbb, 0xcc, 0xdd]); // ack + FCS
        buf.resize(buf.len().next_multiple_of(8), 0);
        buf.extend(desc(10, 0, false, 0));
        buf.extend([0x80, 0x00, 0, 0, 0, 0, 0, 0, 0, 0]);

        let pkts = split(&buf);
        assert_eq!(pkts.len(), 2);
        assert_eq!(frame_kind(u16::from_le_bytes([pkts[0].frame[0], pkts[0].frame[1]])), "ack");
        assert_eq!(pkts[0].dbm, Some(-50));
        assert_eq!(pkts[0].frame.len(), 14);
        assert_eq!(pkts[1].dbm, None);
        assert_eq!(frame_kind(0x0080), "beacon");
    }

    #[test]
    fn stops_on_truncated_frame() {
        let mut buf = desc(100, 0, false, 0);
        buf.extend([0; 10]);
        assert!(split(&buf).is_empty());
    }

    #[test]
    fn reads_probe_request_ssid() {
        let mut f = vec![0x40, 0x00];
        f.extend([0; 22]);
        f.extend([0, 5]);
        f.extend(b"hello");
        f.extend([1, 1, 0x82]);
        f.extend([0; 4]);
        assert_eq!(ssid(&f).as_deref(), Some("hello"));
    }

    #[test]
    fn names_rates() {
        assert_eq!(rate_name(0), "1M");
        assert_eq!(rate_name(11), "54M");
        assert_eq!(rate_name(12), "MCS0");
        assert_eq!(rate_name(44), "VHT1SS-MCS0");
    }
}
