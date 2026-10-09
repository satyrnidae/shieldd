//! TX descriptor and bulk OUT, as rtw88 tx.c and usb.c.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use nusb::transfer::{Buffer, Bulk, Out};
use tracing::{debug, trace};

use crate::usb::Rtl;

pub const DESC_SIZE: usize = 40;
const TIMEOUT: Duration = Duration::from_millis(500);
/// Data frames give up on the bulk OUT sooner: a stuck queue shouldn't stall the radio loop
const DATA_TIMEOUT: Duration = Duration::from_millis(50);
const USB2_PACKET: usize = 512;

pub const QSEL_BEACON: u8 = 16;
pub const QSEL_HIGH: u8 = 17;
pub const QSEL_MGMT: u8 = 18;

pub const RATE_6M: u8 = 4;
#[allow(dead_code)] // Was the data rate; 6M since the queue jams (bss.rs), kept for tests and tuning
pub const RATE_24M: u8 = 8;
const RATEID_G: u32 = 7;

/// What rtw_tx_pkt_info carries for the frames we send
#[derive(Clone, Copy, Debug)]
pub struct TxInfo {
    pub qsel: u8,
    pub rate: u8,
    /// Hardware sequence numbers
    pub hw_seq: bool,
    /// Hardware CCMP: the frame carries our CCMP header, the chip encrypts and appends the MIC
    pub ccmp: bool,
    /// Firmware TX report w/ this sequence number, bits 7:2; filled in by Tx::send
    pub report: Option<u8>,
    /// Drop the frame after this many retries instead of REG_RETRY_LIMIT's 48
    pub retry_limit: Option<u8>,
    /// Ask for a TX report and count the frame in flight until it comes; unicast only
    pub track: bool,
}

impl TxInfo {
    /// Management frames: fixed OFDM rate, no fallback. The controller ignores CCK.
    pub fn mgmt() -> Self {
        Self { qsel: QSEL_MGMT, rate: RATE_6M, hw_seq: true, ccmp: false, report: None, retry_limit: None, track: true }
    }

    pub fn beacon() -> Self {
        Self {
            qsel: QSEL_BEACON,
            rate: RATE_6M,
            hw_seq: true,
            ccmp: false,
            report: None,
            retry_limit: None,
            track: false,
        }
    }

    /// Data on a TID queue
    pub fn data(tid: u8, rate: u8, ccmp: bool) -> Self {
        Self { qsel: tid & 0xf, rate, hw_seq: true, ccmp, report: None, retry_limit: None, track: true }
    }
}

/// rtw_tx_fill_tx_desc + rtw8821a_fill_txdesc_checksum
pub fn desc(info: TxInfo, frame: &[u8]) -> [u8; DESC_SIZE] {
    let bmc = frame.len() >= 10 && frame[4] & 1 != 0;
    let mut w = [0u32; 10];
    w[0] = frame.len() as u32 & 0xffff
        | (DESC_SIZE as u32) << 16
        | (bmc as u32) << 24
        | 1 << 26 // LS
        | (info.hw_seq as u32) << 31; // DISQSELSEQ
    w[1] = (info.qsel as u32 & 0x1f) << 8
        | RATEID_G << 16
        | if info.ccmp { 3 << 22 } else { 0 }
        | ((info.qsel == QSEL_HIGH) as u32) << 29;
    // SPE_RPT
    w[2] = (info.report.is_some() as u32) << 19;
    w[3] = 1 << 8 // USE_RATE
        | 1 << 10; // DISDATAFB
    // DATARATE, and old_datarate_fb_limit
    w[4] = info.rate as u32 & 0x7f | 0x1f << 8;
    // RTY_LMT_EN, DATA_RTY_LMT; halmac's 88xx layout, rtw88 never sets them
    if let Some(n) = info.retry_limit {
        w[4] |= 1 << 17 | (n as u32 & 0x3f) << 18;
    }
    // SW_DEFINE, echoed in the report
    w[6] = info.report.unwrap_or(0) as u32;
    w[8] = (info.hw_seq as u32) << 15;

    let mut d = [0u8; DESC_SIZE];
    for (i, v) in w.iter().enumerate() {
        d[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    // XOR of the first 16 half-words, into w7[15:0]
    let sum = d[..32].chunks(2).fold(0u16, |acc, c| acc ^ u16::from_le_bytes([c[0], c[1]]));
    d[28..30].copy_from_slice(&sum.to_le_bytes());
    d
}

/// Bulk OUT endpoint for a qsel, rtw_usb_parse's mapping for 2-4 endpoints
pub fn endpoint_for(rtl: &Rtl, qsel: u8) -> u8 {
    // rqpn_table_8821a, as indices into the bulk OUT list: high 0, normal 1, low 2, extra 3
    let n = rtl.bulk_out.len();
    let idx = match (n, qsel) {
        (_, QSEL_BEACON | QSEL_HIGH) => 0,
        (2, QSEL_MGMT) => 0,
        (3, QSEL_MGMT) => 0,
        (_, QSEL_MGMT) => 3,
        // TIDs: 0/3 BE, 1/2 BK, 4/5 VI, 6/7 VO
        (2, 4..=7) => 0,
        (2, _) => 1,
        (3, 6 | 7) => 0,
        (3, 4 | 5) => 1,
        (3, _) => 2,
        (_, 4..=7) => 1,
        (_, _) => 2,
    };
    rtl.bulk_out[idx]
}

/// C2H_CCX_TX_RPT
const C2H_CCX_TX_RPT: u8 = 0x03;
/// Frames waiting for a report; past this, the oldest counts as unreported
const MAX_PENDING: usize = 64;
/// A report later than this isn't coming
const REPORT_TIMEOUT: Duration = Duration::from_secs(3);
/// Log frames that needed at least this many retries
const LOG_RETRIES: u8 = 8;

struct Pending {
    sn: u8,
    sent: Instant,
    fc: u16,
}

/// Firmware TX reports since the last take_stats
#[derive(Debug, Default, Clone, Copy)]
pub struct TxStats {
    pub acked: u32,
    pub failed: u32,
    /// Sent w/ a report request, never reported
    pub unreported: u32,
    /// Retries over all reported frames, and the most any one needed
    pub retries: u32,
    pub max_retries: u8,
}

pub struct Tx {
    eps: Vec<(u8, nusb::Endpoint<Bulk, Out>)>,
    next_sn: u8,
    pending: VecDeque<Pending>,
    stats: TxStats,
}

impl Tx {
    pub fn new(rtl: &Rtl) -> Result<Self> {
        let eps =
            rtl.bulk_out.iter().map(|&a| Ok((a, rtl.interface().endpoint::<Bulk, Out>(a)?))).collect::<Result<_>>()?;
        Ok(Self { eps, next_sn: 0, pending: VecDeque::new(), stats: TxStats::default() })
    }

    fn ep(&mut self, addr: u8) -> &mut nusb::Endpoint<Bulk, Out> {
        &mut self.eps.iter_mut().find(|(a, _)| *a == addr).expect("bulk OUT endpoint").1
    }

    /// Descriptor + frame to the queue's endpoint, blocking until the USB transfer completes
    pub fn send(&mut self, rtl: &Rtl, mut info: TxInfo, frame: &[u8]) -> Result<()> {
        // Unicast only: nothing ACKs a broadcast
        if info.track && info.qsel != QSEL_BEACON && frame.len() >= 10 && frame[4] & 1 == 0 {
            self.next_sn = self.next_sn.wrapping_add(4) & 0xfc;
            info.report = Some(self.next_sn);
            if self.pending.len() >= MAX_PENDING {
                self.pending.pop_front();
                self.stats.unreported += 1;
            }
            let fc = u16::from_le_bytes([frame[0], frame[1]]);
            self.pending.push_back(Pending { sn: self.next_sn, sent: Instant::now(), fc });
        }
        let mut buf = Buffer::new(DESC_SIZE + frame.len());
        buf.extend_from_slice(&desc(info, frame));
        buf.extend_from_slice(frame);
        let zlp = buf.len().is_multiple_of(USB2_PACKET);
        let timeout = if info.qsel < QSEL_BEACON { DATA_TIMEOUT } else { TIMEOUT };
        let ep = self.ep(endpoint_for(rtl, info.qsel));
        let done = ep.transfer_blocking(buf, timeout);
        if let Err(e) = done.status {
            bail!("bulk OUT: {e}");
        }
        if zlp {
            // URB_ZERO_PACKET, which WinUSB doesn't do by itself
            let done = ep.transfer_blocking(Buffer::new(0), timeout);
            if let Err(e) = done.status {
                bail!("bulk OUT zero-length packet: {e}");
            }
        }
        Ok(())
    }
}

impl Tx {
    /// A C2H packet from the RX path, id byte first. Returns false if it isn't a TX report.
    pub fn on_c2h(&mut self, c2h: &[u8]) -> bool {
        if c2h.first() != Some(&C2H_CCX_TX_RPT) || c2h.len() < 2 + 7 {
            return false;
        }
        let payload = &c2h[2..];
        // Status 0x80 is retry limit, 0x40 lifetime; payload[2] is the retry count
        let (sn, failed, retries) = (payload[6] & 0xfc, payload[0] & 0xc0 != 0, payload[2] & 0x3f);
        // Each hardware queue reports in its own order, so match by sn only
        let Some(i) = self.pending.iter().position(|p| p.sn == sn) else {
            debug!(sn, payload = ?payload, "TX report for nothing pending");
            return true;
        };
        let p = self.pending.remove(i).expect("found above");
        self.stats.retries += retries as u32;
        self.stats.max_retries = self.stats.max_retries.max(retries);
        if failed {
            self.stats.failed += 1;
        } else {
            self.stats.acked += 1;
        }
        if failed || retries >= LOG_RETRIES {
            let kind = crate::rx::frame_kind(p.fc);
            let ms = p.sent.elapsed().as_millis() as u64;
            if failed {
                debug!(kind, sn, ms, retries, payload = ?payload, "TX not acked");
            } else {
                trace!(kind, sn, ms, retries, payload = ?payload, "TX struggled");
            }
        }
        true
    }

    /// Data frames sent in the last `window` w/o a report yet: roughly what's sitting in the chip
    pub fn data_in_flight(&self, window: Duration) -> usize {
        self.pending.iter().filter(|p| (p.fc >> 2) & 3 == 2 && p.sent.elapsed() < window).count()
    }

    /// Stats since the last call; frames still waiting past REPORT_TIMEOUT count as unreported
    pub fn take_stats(&mut self) -> TxStats {
        let before = self.pending.len();
        self.pending.retain(|p| p.sent.elapsed() < REPORT_TIMEOUT);
        self.stats.unreported += (before - self.pending.len()) as u32;
        std::mem::take(&mut self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_makes_first_32_bytes_xor_to_zero() {
        let frame = [0x50, 0, 0, 0, 0x00, 0x04, 0x4b, 0x21, 0x92, 0x06];
        let d = desc(TxInfo::mgmt(), &frame);
        let x = d[..32].chunks(2).fold(0u16, |a, c| a ^ u16::from_le_bytes([c[0], c[1]]));
        assert_eq!(x, 0);
    }

    #[test]
    fn fills_size_offset_qsel_and_rate() {
        let frame = [0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let d = desc(TxInfo::beacon(), &frame);
        let w0 = u32::from_le_bytes(d[0..4].try_into().unwrap());
        let w1 = u32::from_le_bytes(d[4..8].try_into().unwrap());
        let w4 = u32::from_le_bytes(d[16..20].try_into().unwrap());
        assert_eq!(w0 & 0xffff, 10);
        assert_eq!((w0 >> 16) & 0xff, 40);
        assert_ne!(w0 & 1 << 24, 0, "broadcast sets BMC");
        assert_eq!((w1 >> 8) & 0x1f, 16);
        assert_eq!(w4 & 0x7f, 4);
    }

    #[test]
    fn report_request_sets_spe_rpt_and_sw_define() {
        let frame = [0x88, 0x02, 0, 0, 0x00, 0x04, 0x4b, 0x21, 0x92, 0x06];
        let d = desc(TxInfo { report: Some(0x24), ..TxInfo::data(0, RATE_24M, true) }, &frame);
        let w2 = u32::from_le_bytes(d[8..12].try_into().unwrap());
        let w6 = u32::from_le_bytes(d[24..28].try_into().unwrap());
        assert_ne!(w2 & 1 << 19, 0);
        assert_eq!(w6 & 0xfff, 0x24);
        assert_eq!(desc(TxInfo::data(0, RATE_24M, true), &frame)[8..12], [0; 4]);
    }

    #[test]
    fn retry_limit_lands_in_w4() {
        let frame = [0x88, 0x02, 0, 0, 0x00, 0x04, 0x4b, 0x21, 0x92, 0x06];
        let d = desc(TxInfo { retry_limit: Some(12), ..TxInfo::data(0, RATE_6M, true) }, &frame);
        let w4 = u32::from_le_bytes(d[16..20].try_into().unwrap());
        assert_eq!(w4 & 0x7f, RATE_6M as u32);
        assert_ne!(w4 & 1 << 17, 0);
        assert_eq!((w4 >> 18) & 0x3f, 12);
    }
}
