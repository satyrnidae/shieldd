//! Frame header, element framing and the connect handshake.
//!
//! Layouts follow ozwpan's `ozprotocol.h`. Everything is packed and little endian.

use std::time::Duration;

pub const ETHERTYPE: u16 = 0x892E;
pub const PROTOCOL_VERSION: u8 = 1;

/// Bits of [`Header::control`].
pub mod ctl {
    pub const VERSION_MASK: u8 = 0x0C;
    pub const VERSION_SHIFT: u8 = 2;
    pub const ACK: u8 = 0x10;
    /// Frame is an isoc-large unit, not a list of elements
    pub const ISOC: u8 = 0x20;
    pub const MORE_DATA: u8 = 0x40;
    pub const ACK_REQUESTED: u8 = 0x80;
}

/// Element type IDs.
pub mod elt {
    pub const CONNECT_REQ: u8 = 0x06;
    pub const CONNECT_RSP: u8 = 0x07;
    pub const DISCONNECT: u8 = 0x08;
    pub const UPDATE_PARAM_REQ: u8 = 0x11;
    pub const FAREWELL_REQ: u8 = 0x12;
    pub const APP_DATA: u8 = 0x31;
}

/// Bits of the connect `mode` field.
pub mod mode {
    pub const MASK: u8 = 0x0F;
    pub const POLLED: u8 = 0x00;
    pub const TRIGGERED: u8 = 0x01;
    pub const ISOC_NO_ELTS: u8 = 0x40;
    pub const ISOC_ANYTIME: u8 = 0x80;
}

/// Application IDs carried in APP_DATA elements and the connect `apps` bitmap.
pub mod app {
    pub const USB: u8 = 1;
    pub const SERIAL: u8 = 4;
}

/// Connect response status codes.
pub mod status {
    pub const SUCCESS: u8 = 0;
    pub const INVALID_PARAM: u8 = 1;
    pub const TOO_MANY_PDS: u8 = 2;
    pub const NOT_ALLOWED: u8 = 4;
    pub const SESSION_MISMATCH: u8 = 5;
    pub const SESSION_TEARDOWN: u8 = 6;
}

/// Fallback when the device doesn't state a max frame size (`OZ_MAX_TX_SIZE`)
pub const DEFAULT_MAX_TX: usize = 760;

//region Header

/// The 6-byte Ozmo header that follows the Ethernet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub control: u8,
    /// Low byte of the last packet number seen from the peer
    pub last_pkt_num: u8,
    pub pkt_num: u32,
}

impl Header {
    pub const LEN: usize = 6;

    /// Splits `frame` into its header and the rest.
    pub fn parse(frame: &[u8]) -> Option<(Header, &[u8])> {
        let (h, rest) = frame.split_first_chunk::<{ Self::LEN }>()?;
        let hdr = Header {
            control: h[0],
            last_pkt_num: h[1],
            pkt_num: u32::from_le_bytes([h[2], h[3], h[4], h[5]]),
        };
        Some((hdr, rest))
    }

    pub fn version(&self) -> u8 {
        (self.control & ctl::VERSION_MASK) >> ctl::VERSION_SHIFT
    }

    pub fn has(&self, flag: u8) -> bool {
        self.control & flag != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.control);
        out.push(self.last_pkt_num);
        out.extend_from_slice(&self.pkt_num.to_le_bytes());
    }
}

/// Control byte with the protocol version set and nothing else
pub const fn base_control() -> u8 {
    PROTOCOL_VERSION << ctl::VERSION_SHIFT
}

//endregion

//region Elements

/// One `{type, len, body}` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element<'a> {
    pub kind: u8,
    pub body: &'a [u8],
}

/// Iterates the elements after a header. Stops at a truncated element or at a
/// zero type byte, which is Ethernet minimum-size padding in practice.
#[derive(Debug, Clone)]
pub struct Elements<'a> {
    rest: &'a [u8],
}

impl<'a> Elements<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Elements { rest: body }
    }
}

impl<'a> Iterator for Elements<'a> {
    type Item = Element<'a>;

    fn next(&mut self) -> Option<Element<'a>> {
        let (&[kind, len], rest) = self.rest.split_first_chunk::<2>()?;
        if kind == 0 {
            self.rest = &[];
            return None;
        }
        let Some((body, rest)) = rest.split_at_checked(len as usize) else {
            self.rest = &[];
            return None;
        };
        self.rest = rest;
        Some(Element { kind, body })
    }
}

/// Appends a whole element. Panics if `body` doesn't fit the 1-byte length,
/// which would be a bug in the caller rather than bad input.
pub fn push_element(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    let len = u8::try_from(body.len()).expect("element body over 255 bytes");
    out.push(kind);
    out.push(len);
    out.extend_from_slice(body);
}

//endregion

//region Connect

/// CONNECT_REQ body, sent by the device to start or resume a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectReq {
    pub mode: u8,
    pub pd_info: u8,
    pub session_id: u8,
    /// Idle time before the host may consider the device asleep, in 100 ms units
    pub presleep: u8,
    pub ms_isoc_latency: u8,
    pub host_vendor: u8,
    pub keep_alive: u8,
    pub apps: u16,
    pub max_len_div16: u8,
    pub ms_per_isoc: u8,
}

impl ConnectReq {
    pub const LEN: usize = 29;

    pub fn parse(body: &[u8]) -> Option<ConnectReq> {
        let b = body.get(..Self::LEN)?;
        Some(ConnectReq {
            mode: b[0],
            // b[1..17] reserved
            pd_info: b[17],
            session_id: b[18],
            presleep: b[19],
            ms_isoc_latency: b[20],
            host_vendor: b[21],
            keep_alive: b[22],
            apps: u16::from_le_bytes([b[23], b[24]]),
            max_len_div16: b[25],
            ms_per_isoc: b[26],
            // b[27..29] reserved
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; Self::LEN];
        b[0] = self.mode;
        b[17] = self.pd_info;
        b[18] = self.session_id;
        b[19] = self.presleep;
        b[20] = self.ms_isoc_latency;
        b[21] = self.host_vendor;
        b[22] = self.keep_alive;
        b[23..25].copy_from_slice(&self.apps.to_le_bytes());
        b[25] = self.max_len_div16;
        b[26] = self.ms_per_isoc;
        b
    }

    pub fn triggered(&self) -> bool {
        self.mode & mode::MASK == mode::TRIGGERED
    }

    pub fn max_tx_size(&self) -> usize {
        match self.max_len_div16 {
            0 => DEFAULT_MAX_TX,
            n => usize::from(n) << 4,
        }
    }

    pub fn presleep_time(&self) -> Duration {
        presleep_time(self.presleep)
    }

    pub fn keep_alive_time(&self) -> Option<Duration> {
        keep_alive_time(self.keep_alive)
    }
}

/// `presleep` is in 100 ms units. Linux falls back to 11 of them when it's 0.
pub fn presleep_time(presleep: u8) -> Duration {
    Duration::from_millis(100 * u64::from(if presleep == 0 { 11 } else { presleep }))
}

/// Top two bits pick the unit, low six bits are the count. None = no keepalive.
pub fn keep_alive_time(keep_alive: u8) -> Option<Duration> {
    let n = u64::from(keep_alive & 0x3F);
    let secs = match keep_alive & 0xC0 {
        0x40 => n,
        0x80 => n * 60,
        0xC0 => n * 60 * 60,
        _ => n * 60 * 60 * 24 * 20, // "special": units of 20 days, per ozwpan
    };
    (secs != 0).then(|| Duration::from_secs(secs))
}

/// CONNECT_RSP body, the host's answer to [`ConnectReq`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectRsp {
    pub mode: u8,
    pub status: u8,
    pub session_id: u8,
    pub apps: u16,
}

impl ConnectRsp {
    pub const LEN: usize = 12;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = self.mode;
        b[1] = self.status;
        // b[2..5] reserved
        b[5] = self.session_id;
        b[6..8].copy_from_slice(&self.apps.to_le_bytes());
        // b[8..12] reserved
        b
    }

    pub fn parse(body: &[u8]) -> Option<ConnectRsp> {
        let b = body.get(..Self::LEN)?;
        Some(ConnectRsp {
            mode: b[0],
            status: b[1],
            session_id: b[5],
            apps: u16::from_le_bytes([b[6], b[7]]),
        })
    }
}

/// UPDATE_PARAM_REQ body: the device changing its sleep timing mid-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateParam {
    pub presleep: u8,
    pub host_vendor: u8,
    pub keep_alive: u8,
}

impl UpdateParam {
    pub const LEN: usize = 20;

    pub fn parse(body: &[u8]) -> Option<UpdateParam> {
        let b = body.get(..Self::LEN)?;
        Some(UpdateParam { presleep: b[16], host_vendor: b[18], keep_alive: b[19] })
    }
}

//endregion

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // SHIELD Tablet handshake capture, frame 5
    pub(crate) fn tablet_connect_req_frame() -> Vec<u8> {
        let req = ConnectReq {
            mode: 0xC1,
            pd_info: 0x02,
            session_id: 0,
            presleep: 0x23,
            ms_isoc_latency: 0x84,
            host_vendor: 0x04,
            keep_alive: 0x46,
            apps: 0x0013,
            max_len_div16: 0x21,
            ms_per_isoc: 0x04,
        };
        let mut f = Vec::new();
        Header { control: 0x84, last_pkt_num: 0, pkt_num: 7_446_941 }.write(&mut f);
        push_element(&mut f, elt::CONNECT_REQ, &req.encode());
        f
    }

    #[test]
    fn header_round_trip() {
        let (h, rest) = Header::parse(&[0x94, 0x01, 0x9f, 0xa1, 0x71, 0x00, 0xAA]).unwrap();
        assert_eq!(h, Header { control: 0x94, last_pkt_num: 1, pkt_num: 7_446_943 });
        assert_eq!(h.version(), 1);
        assert!(h.has(ctl::ACK) && h.has(ctl::ACK_REQUESTED));
        assert_eq!(rest, &[0xAA]);
        let mut out = Vec::new();
        h.write(&mut out);
        assert_eq!(out, [0x94, 0x01, 0x9f, 0xa1, 0x71, 0x00]);
        assert!(Header::parse(&[0; 5]).is_none());
    }

    #[test]
    fn connect_req_from_tablet_capture() {
        let f = tablet_connect_req_frame();
        let (_, body) = Header::parse(&f).unwrap();
        let e: Vec<_> = Elements::new(body).collect();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, elt::CONNECT_REQ);
        let req = ConnectReq::parse(e[0].body).unwrap();
        assert!(req.triggered());
        assert_eq!(req.apps, 0x0013);
        assert_eq!(req.max_tx_size(), 528);
        assert_eq!(req.presleep_time(), Duration::from_millis(3500));
        assert_eq!(req.keep_alive_time(), Some(Duration::from_secs(6)));
    }

    #[test]
    fn connect_rsp_matches_tablet_bytes() {
        let rsp = ConnectRsp { mode: 0xC1, status: status::SUCCESS, session_id: 2, apps: 0x0012 };
        let mut f = Vec::new();
        push_element(&mut f, elt::CONNECT_RSP, &rsp.encode());
        assert_eq!(f, hex("070cc10000000002120000000000"));
        assert_eq!(ConnectRsp::parse(&f[2..]), Some(rsp));
    }

    #[test]
    fn elements_stop_on_truncation_and_padding() {
        let body = [0x31, 0x02, 0xAA, 0xBB, 0x08, 0x00, 0x00, 0x00, 0x00];
        let e: Vec<_> = Elements::new(&body).collect();
        assert_eq!(e, [
            Element { kind: 0x31, body: &[0xAA, 0xBB] },
            Element { kind: 0x08, body: &[] },
        ]);
        // length runs past the end
        assert_eq!(Elements::new(&[0x31, 0x05, 0xAA]).count(), 0);
        assert_eq!(Elements::new(&[0x31]).count(), 0);
    }

    #[test]
    fn keep_alive_units() {
        assert_eq!(keep_alive_time(0x46), Some(Duration::from_secs(6)));
        assert_eq!(keep_alive_time(0x82), Some(Duration::from_secs(120)));
        assert_eq!(keep_alive_time(0x40), None);
        assert_eq!(presleep_time(0), Duration::from_millis(1100));
    }

    pub(crate) fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
