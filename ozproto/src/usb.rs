//! USB application (app ID 1) payloads carried in APP_DATA elements.
//!
//! The host sends control requests; the device answers them and pushes endpoint
//! data on its own. Unlike a real setup packet, VENDOR_CLASS carries no
//! wLength: the data length is whatever is left in the element.

use crate::wire::{app, elt, push_element};

/// USB message types (`type` byte after `app_id` and `elt_seq_num`).
pub mod kind {
    pub const GET_DESC_REQ: u8 = 1;
    pub const GET_DESC_RSP: u8 = 2;
    pub const SET_CONFIG_REQ: u8 = 3;
    pub const SET_CONFIG_RSP: u8 = 4;
    pub const SET_INTERFACE_REQ: u8 = 5;
    pub const SET_INTERFACE_RSP: u8 = 6;
    pub const VENDOR_CLASS_REQ: u8 = 7;
    pub const VENDOR_CLASS_RSP: u8 = 8;
    pub const GET_STATUS_REQ: u8 = 9;
    pub const GET_STATUS_RSP: u8 = 10;
    pub const CLEAR_FEATURE_REQ: u8 = 11;
    pub const CLEAR_FEATURE_RSP: u8 = 12;
    pub const SET_FEATURE_REQ: u8 = 13;
    pub const SET_FEATURE_RSP: u8 = 14;
    pub const ENDPOINT_DATA: u8 = 23;
}

/// ENDPOINT_DATA `format` values.
pub mod format {
    pub const MASK: u8 = 0x0F;
    pub const MULTIPLE_FIXED: u8 = 0x1;
    pub const MULTIPLE_VAR: u8 = 0x2;
    pub const ISOC_FIXED: u8 = 0x3;
    pub const ISOC_VAR: u8 = 0x4;
    pub const FRAGMENTED: u8 = 0x5;
    pub const ISOC_LARGE: u8 = 0x7;
}

/// Largest descriptor chunk the device is asked for at once (ozwpan caps at 200)
pub const MAX_DESC_CHUNK: u16 = 200;

/// A host -> device request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    GetDesc {
        req_id: u8,
        req_type: u8,
        desc_type: u8,
        index: u8,
        w_index: u16,
        offset: u16,
        size: u16,
    },
    SetConfig {
        req_id: u8,
        config: u8,
    },
    SetInterface {
        req_id: u8,
        interface: u8,
        alt: u8,
    },
    /// Anything that isn't a standard request, e.g. HID GET/SET_REPORT
    VendorClass {
        req_id: u8,
        req_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
    },
    Feature {
        set: bool,
        req_id: u8,
        recipient: u8,
        index: u8,
        feature: u16,
    },
}

impl Request {
    pub fn req_id(&self) -> u8 {
        match *self {
            Request::GetDesc { req_id, .. }
            | Request::SetConfig { req_id, .. }
            | Request::SetInterface { req_id, .. }
            | Request::VendorClass { req_id, .. }
            | Request::Feature { req_id, .. } => req_id,
        }
    }

    /// The complete APP_DATA element, with `seq` as its element sequence number.
    pub fn encode(&self, seq: u8) -> Vec<u8> {
        let mut b = vec![app::USB, seq];
        match self {
            &Request::GetDesc { req_id, req_type, desc_type, index, w_index, offset, size } => {
                b.extend([kind::GET_DESC_REQ, req_id]);
                b.extend(offset.to_le_bytes());
                b.extend(size.to_le_bytes());
                b.extend([req_type, desc_type]);
                b.extend(w_index.to_le_bytes());
                b.push(index);
            }
            &Request::SetConfig { req_id, config } => {
                b.extend([kind::SET_CONFIG_REQ, req_id, config]);
            }
            &Request::SetInterface { req_id, interface, alt } => {
                b.extend([kind::SET_INTERFACE_REQ, req_id, interface, alt]);
            }
            Request::VendorClass { req_id, req_type, request, value, index, data } => {
                b.extend([kind::VENDOR_CLASS_REQ, *req_id, *req_type, *request]);
                b.extend(value.to_le_bytes());
                b.extend(index.to_le_bytes());
                b.extend_from_slice(data);
            }
            &Request::Feature { set, req_id, recipient, index, feature } => {
                let k = if set { kind::SET_FEATURE_REQ } else { kind::CLEAR_FEATURE_REQ };
                b.extend([k, req_id, recipient, index]);
                b.extend(feature.to_le_bytes());
            }
        }
        let mut out = Vec::with_capacity(b.len() + 2);
        push_element(&mut out, elt::APP_DATA, &b);
        out
    }
}

/// A device -> host USB message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    GetDesc {
        req_id: u8,
        offset: u16,
        total_size: u16,
        rcode: u8,
        data: Vec<u8>,
    },
    /// SET_CONFIG, SET_INTERFACE and the feature requests: only a status
    Status {
        kind: u8,
        req_id: u8,
        rcode: u8,
    },
    VendorClass {
        req_id: u8,
        rcode: u8,
        data: Vec<u8>,
    },
    EndpointData(EndpointData),
    Unknown {
        kind: u8,
        body: Vec<u8>,
    },
}

impl Message {
    /// Parses the body of a USB APP_DATA element. Returns the element
    /// sequence number with the message.
    pub fn parse(body: &[u8]) -> Option<(u8, Message)> {
        let (&[app_id, seq, k], rest) = body.split_first_chunk::<3>()?;
        if app_id != app::USB {
            return None;
        }
        let msg = match k {
            kind::GET_DESC_RSP => {
                let (&[req_id, o0, o1, t0, t1, rcode], data) = rest.split_first_chunk::<6>()?;
                Message::GetDesc {
                    req_id,
                    offset: u16::from_le_bytes([o0, o1]),
                    total_size: u16::from_le_bytes([t0, t1]),
                    rcode,
                    data: data.to_vec(),
                }
            }
            kind::SET_CONFIG_RSP | kind::SET_INTERFACE_RSP | kind::SET_FEATURE_RSP | kind::CLEAR_FEATURE_RSP => {
                let &[req_id, rcode, ..] = rest else { return None };
                Message::Status { kind: k, req_id, rcode }
            }
            kind::VENDOR_CLASS_RSP => {
                let (&[req_id, rcode], data) = rest.split_first_chunk::<2>()?;
                Message::VendorClass { req_id, rcode, data: data.to_vec() }
            }
            kind::ENDPOINT_DATA => {
                let (&[endpoint, format], payload) = rest.split_first_chunk::<2>()?;
                Message::EndpointData(EndpointData { endpoint, format, payload: payload.to_vec() })
            }
            _ => Message::Unknown { kind: k, body: rest.to_vec() },
        };
        Some((seq, msg))
    }
}

/// ENDPOINT_DATA: an interrupt/bulk/isoc payload pushed by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointData {
    /// USB endpoint address, direction bit included (0x81 = HID interrupt IN)
    pub endpoint: u8,
    pub format: u8,
    /// Everything after the format byte
    pub payload: Vec<u8>,
}

impl EndpointData {
    /// Splits fixed-size formats into their units. MULTIPLE_FIXED is
    /// `unit_size, data...`; ISOC_FIXED adds a frame number byte.
    pub fn units(&self) -> Option<std::slice::ChunksExact<'_, u8>> {
        let (unit, data) = match self.format & format::MASK {
            format::MULTIPLE_FIXED => {
                let (&[unit], data) = self.payload.split_first_chunk::<1>()?;
                (unit, data)
            }
            format::ISOC_FIXED => {
                let (&[unit, _frame], data) = self.payload.split_first_chunk::<2>()?;
                (unit, data)
            }
            _ => return None,
        };
        (unit != 0).then(|| data.chunks_exact(usize::from(unit)))
    }
}

/// Element sequence tracking for one app. Mirrors ozwpan: TX numbers start at
/// 0 and skip 0 when they wrap; RX accepts 0 always and otherwise only numbers
/// ahead of the last one, within half a cycle.
#[derive(Debug, Clone, Default)]
pub struct SeqState {
    tx: u8,
    rx: u8,
}

impl SeqState {
    pub fn next_tx(&mut self) -> u8 {
        let s = self.tx;
        self.tx = match self.tx.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        s
    }

    /// True if `seq` is new; records it.
    pub fn accept_rx(&mut self, seq: u8) -> bool {
        if seq != 0 && self.rx.wrapping_sub(seq) & 0x80 == 0 {
            return false;
        }
        self.rx = seq;
        true
    }
}

//region Request helpers

pub const DESC_DEVICE: u8 = 0x01;
pub const DESC_CONFIG: u8 = 0x02;
pub const DESC_STRING: u8 = 0x03;
pub const DESC_HID_REPORT: u8 = 0x22;

/// bmRequestType: device-to-host, standard, device
pub const RT_IN_STD_DEVICE: u8 = 0x80;
/// bmRequestType: device-to-host, standard, interface
pub const RT_IN_STD_INTERFACE: u8 = 0x81;
/// bmRequestType: host-to-device, class, interface
pub const RT_OUT_CLASS_INTERFACE: u8 = 0x21;
/// bmRequestType: device-to-host, class, interface
pub const RT_IN_CLASS_INTERFACE: u8 = 0xA1;

pub const HID_GET_REPORT: u8 = 0x01;
pub const HID_SET_IDLE: u8 = 0x0A;
pub const HID_SET_REPORT: u8 = 0x09;

pub const REPORT_INPUT: u8 = 1;
pub const REPORT_OUTPUT: u8 = 2;
pub const REPORT_FEATURE: u8 = 3;

impl Request {
    pub fn get_desc(req_id: u8, req_type: u8, desc_type: u8, index: u8, w_index: u16, offset: u16, size: u16) -> Self {
        Request::GetDesc { req_id, req_type, desc_type, index, w_index, offset, size: size.min(MAX_DESC_CHUNK) }
    }

    pub fn hid_get_report(req_id: u8, interface: u16, report_type: u8, report_id: u8) -> Self {
        Request::VendorClass {
            req_id,
            req_type: RT_IN_CLASS_INTERFACE,
            request: HID_GET_REPORT,
            value: u16::from_le_bytes([report_id, report_type]),
            index: interface,
            data: Vec::new(),
        }
    }

    /// `data` starts with the report ID, as on the wire
    pub fn hid_set_report(req_id: u8, interface: u16, report_type: u8, data: Vec<u8>) -> Self {
        let report_id = data.first().copied().unwrap_or(0);
        Request::VendorClass {
            req_id,
            req_type: RT_OUT_CLASS_INTERFACE,
            request: HID_SET_REPORT,
            value: u16::from_le_bytes([report_id, report_type]),
            index: interface,
            data,
        }
    }

    pub fn hid_set_idle(req_id: u8, interface: u16) -> Self {
        Request::VendorClass {
            req_id,
            req_type: RT_OUT_CLASS_INTERFACE,
            request: HID_SET_IDLE,
            value: 0,
            index: interface,
            data: Vec::new(),
        }
    }
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::hex;

    // Expected bytes are APP_DATA elements from a SHIELD Tablet handshake capture

    #[test]
    fn get_desc_request() {
        let r = Request::get_desc(1, RT_IN_STD_DEVICE, DESC_DEVICE, 0, 0, 0, 8);
        assert_eq!(r.encode(0), hex("310d01000101000008008001000000"));
        let r = Request::get_desc(0x0b, RT_IN_STD_INTERFACE, DESC_HID_REPORT, 0, 0, 0xc8, 0x29);
        assert_eq!(r.encode(0x0b), hex("310d010b010bc80029008122000000"));
    }

    #[test]
    fn hid_class_requests() {
        assert_eq!(Request::hid_set_idle(0x0a, 0).encode(0x09), hex("310a0109070a210a00000000"));
        assert_eq!(Request::hid_get_report(0x0c, 0, REPORT_INPUT, 0x01).encode(0x0c), hex("310a010c070ca10101010000"));
        assert_eq!(
            Request::hid_get_report(0x0f, 0, REPORT_FEATURE, 0xfc).encode(0x0f),
            hex("310a010f070fa101fc030000")
        );
        assert_eq!(
            Request::hid_set_report(0x1b, 0, REPORT_FEATURE, vec![0xfd, 0x0a]).encode(0x1b),
            hex("310c011b071b2109fd030000fd0a")
        );
    }

    #[test]
    fn set_config_and_interface() {
        assert_eq!(Request::SetConfig { req_id: 8, config: 1 }.encode(7), hex("31050107030801"));
        assert_eq!(Request::SetInterface { req_id: 0x13, interface: 2, alt: 1 }.encode(0x13), hex("3106011305130201"));
    }

    #[test]
    fn parse_responses() {
        let (seq, m) = Message::parse(&hex("0105020600003400003403")).unwrap();
        assert_eq!(seq, 5);
        assert_eq!(m, Message::GetDesc { req_id: 6, offset: 0, total_size: 0x34, rcode: 0, data: vec![0x34, 0x03] });

        let (_, m) = Message::parse(&hex("0107040800")).unwrap();
        assert_eq!(m, Message::Status { kind: kind::SET_CONFIG_RSP, req_id: 8, rcode: 0 });

        let (_, m) = Message::parse(&hex("0124081d000a00044b")).unwrap();
        assert_eq!(m, Message::VendorClass { req_id: 0x1d, rcode: 0, data: hex("0a00044b") });

        assert!(Message::parse(&hex("0401")).is_none());
        assert!(Message::parse(&hex("040102")).is_none(), "serial app isn't USB");
    }

    #[test]
    fn endpoint_units() {
        // Mic data, frame 131: ep 0x83, ISOC_FIXED, unit 0x20, frame 0
        let data: String = (0u8..32).map(|b| format!("{b:02x}")).collect();
        let (_, m) = Message::parse(&hex(&format!("01161783032000{data}"))).unwrap();
        let Message::EndpointData(d) = m else { panic!("{m:?}") };
        assert_eq!(d.endpoint, 0x83);
        let units: Vec<_> = d.units().unwrap().collect();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].len(), 32);

        let d = EndpointData { endpoint: 0x81, format: format::MULTIPLE_FIXED, payload: hex("03aabbccddeeff") };
        assert_eq!(d.units().unwrap().collect::<Vec<_>>(), [&hex("aabbcc")[..], &hex("ddeeff")[..]]);
    }

    #[test]
    fn seq_numbers() {
        let mut s = SeqState::default();
        assert_eq!(s.next_tx(), 0);
        assert_eq!(s.next_tx(), 1);
        s.tx = 255;
        assert_eq!(s.next_tx(), 255);
        assert_eq!(s.next_tx(), 1, "wraps past 0");

        let mut s = SeqState::default();
        assert!(s.accept_rx(0));
        assert!(s.accept_rx(1));
        assert!(!s.accept_rx(1), "duplicate");
        assert!(s.accept_rx(2));
        assert!(!s.accept_rx(1), "old");
        assert!(s.accept_rx(0), "zero always accepted");
    }
}
