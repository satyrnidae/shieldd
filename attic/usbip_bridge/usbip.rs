//! USB/IP v1.1.1 wire format, server side. Everything is big endian except
//! the setup packet, which is the raw little-endian USB bytes.
//!
//! Reference: Linux Documentation/usb/usbip_protocol.rst

pub const VERSION: u16 = 0x0111;

const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;

const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// Linux errno values, negated on the wire
pub const ECONNRESET: i32 = 104;
pub const EPIPE: i32 = 32;

pub const SPEED_FULL: u32 = 2;

const HDR: usize = 48;
const ISO_DESC: usize = 16;
const OP_HDR: usize = 8;
const BUSID_LEN: usize = 32;

//region Device info

/// What OP_REP_DEVLIST and OP_REP_IMPORT say about the exported device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    /// (class, subclass, protocol) of each interface's first alt setting
    pub interfaces: Vec<(u8, u8, u8)>,
}

impl DeviceInfo {
    /// Builds the info from a device descriptor and a full config descriptor.
    pub fn from_descriptors(busid: &str, device: &[u8], config: &[u8]) -> Option<Self> {
        let d = device.get(..18)?;
        let mut interfaces = Vec::new();
        let mut off = 0;
        while off + 2 <= config.len() {
            let len = usize::from(config[off]);
            if len < 2 {
                break;
            }
            // interface descriptor, alt setting 0
            if config[off + 1] == 0x04 && len >= 9 && off + 9 <= config.len() && config[off + 3] == 0 {
                interfaces.push((config[off + 5], config[off + 6], config[off + 7]));
            }
            off += len;
        }
        Some(DeviceInfo {
            path: format!("/sys/devices/shieldd/{busid}"),
            busid: busid.to_owned(),
            busnum: 1,
            devnum: 1,
            speed: SPEED_FULL,
            id_vendor: u16::from_le_bytes([d[8], d[9]]),
            id_product: u16::from_le_bytes([d[10], d[11]]),
            bcd_device: u16::from_le_bytes([d[12], d[13]]),
            device_class: d[4],
            device_subclass: d[5],
            device_protocol: d[6],
            configuration_value: 0,
            num_configurations: d[17],
            interfaces,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_fixed_str(out, &self.path, 256);
        put_fixed_str(out, &self.busid, BUSID_LEN);
        out.extend(self.busnum.to_be_bytes());
        out.extend(self.devnum.to_be_bytes());
        out.extend(self.speed.to_be_bytes());
        out.extend(self.id_vendor.to_be_bytes());
        out.extend(self.id_product.to_be_bytes());
        out.extend(self.bcd_device.to_be_bytes());
        out.extend([
            self.device_class,
            self.device_subclass,
            self.device_protocol,
            self.configuration_value,
            self.num_configurations,
            self.interfaces.len() as u8,
        ]);
    }
}

fn put_fixed_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let b = s.as_bytes();
    let n = b.len().min(len - 1);
    out.extend_from_slice(&b[..n]);
    out.resize(out.len() + (len - n), 0);
}

//endregion

//region Requests

/// One isochronous packet descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u8,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    /// OUT payload; empty for IN
    pub data: Vec<u8>,
    /// Empty for non-isochronous transfers
    pub iso: Vec<IsoPacket>,
}

/// A message from the client, before or after import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    DevList,
    Import { busid: String },
    Submit(Submit),
    Unlink { seqnum: u32, unlink_seqnum: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    BadVersion(u16),
    UnknownOp(u16),
    UnknownCommand(u32),
    TooLarge(u32),
}

/// Largest transfer accepted from the client, to bound allocations
const MAX_TRANSFER: u32 = 1 << 20;
const MAX_ISO_PACKETS: u32 = 1024;

/// Parses one message from the front of `buf`. `imported` selects between
/// the setup ops and the URB commands that follow a successful import.
/// Ok(None) means more bytes are needed; otherwise returns the message and
/// how many bytes it used.
pub fn parse(buf: &[u8], imported: bool) -> Result<Option<(Request, usize)>, ParseError> {
    if !imported {
        let Some(h) = buf.get(..OP_HDR) else { return Ok(None) };
        let version = be16(&h[0..]);
        let code = be16(&h[2..]);
        if version != VERSION {
            return Err(ParseError::BadVersion(version));
        }
        return match code {
            OP_REQ_DEVLIST => Ok(Some((Request::DevList, OP_HDR))),
            OP_REQ_IMPORT => {
                let Some(b) = buf.get(OP_HDR..OP_HDR + BUSID_LEN) else { return Ok(None) };
                let end = b.iter().position(|&c| c == 0).unwrap_or(BUSID_LEN);
                let busid = String::from_utf8_lossy(&b[..end]).into_owned();
                Ok(Some((Request::Import { busid }, OP_HDR + BUSID_LEN)))
            }
            other => Err(ParseError::UnknownOp(other)),
        };
    }

    let Some(h) = buf.get(..HDR) else { return Ok(None) };
    let command = be32(&h[0..]);
    let seqnum = be32(&h[4..]);
    match command {
        CMD_SUBMIT => {
            let direction = be32(&h[12..]);
            let len = be32(&h[24..]);
            let npackets = be32(&h[32..]);
            // Non-iso transfers carry 0, or 0xffffffff from some clients
            let npackets = if npackets == u32::MAX { 0 } else { npackets };
            if len > MAX_TRANSFER {
                return Err(ParseError::TooLarge(len));
            }
            if npackets > MAX_ISO_PACKETS {
                return Err(ParseError::TooLarge(npackets));
            }
            let data_len = if direction == DIR_OUT { len as usize } else { 0 };
            let total = HDR + data_len + npackets as usize * ISO_DESC;
            if buf.len() < total {
                return Ok(None);
            }
            let data = buf[HDR..HDR + data_len].to_vec();
            let iso = buf[HDR + data_len..total]
                .as_chunks::<ISO_DESC>()
                .0
                .iter()
                .map(|d| IsoPacket {
                    offset: be32(&d[0..]),
                    length: be32(&d[4..]),
                    actual_length: be32(&d[8..]),
                    status: be32(&d[12..]) as i32,
                })
                .collect();
            let submit = Submit {
                seqnum,
                devid: be32(&h[8..]),
                direction,
                ep: be32(&h[16..]) as u8,
                transfer_flags: be32(&h[20..]),
                transfer_buffer_length: len,
                start_frame: be32(&h[28..]) as i32,
                interval: be32(&h[36..]) as i32,
                setup: h[40..48].try_into().unwrap(),
                data,
                iso,
            };
            Ok(Some((Request::Submit(submit), total)))
        }
        CMD_UNLINK => Ok(Some((Request::Unlink { seqnum, unlink_seqnum: be32(&h[20..]) }, HDR))),
        other => Err(ParseError::UnknownCommand(other)),
    }
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

//endregion

//region Replies

pub fn rep_devlist(devices: &[&DeviceInfo]) -> Vec<u8> {
    let mut out = op_header(OP_REP_DEVLIST, 0);
    out.extend((devices.len() as u32).to_be_bytes());
    for d in devices {
        d.write(&mut out);
        for &(class, subclass, protocol) in &d.interfaces {
            out.extend([class, subclass, protocol, 0]);
        }
    }
    out
}

/// None = refuse the import
pub fn rep_import(device: Option<&DeviceInfo>) -> Vec<u8> {
    match device {
        Some(d) => {
            let mut out = op_header(OP_REP_IMPORT, 0);
            d.write(&mut out);
            out
        }
        None => op_header(OP_REP_IMPORT, 1),
    }
}

fn op_header(code: u16, status: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(OP_HDR);
    out.extend(VERSION.to_be_bytes());
    out.extend(code.to_be_bytes());
    out.extend(status.to_be_bytes());
    out
}

/// RET_SUBMIT. `status` is 0 or a negated errno. For isochronous IN, `data`
/// holds the packets' actual bytes back to back, as Linux sends them.
pub fn ret_submit(seqnum: u32, status: i32, data: &[u8], actual_length: u32, start_frame: i32, iso: &[IsoPacket]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HDR + data.len() + iso.len() * ISO_DESC);
    out.extend(RET_SUBMIT.to_be_bytes());
    out.extend(seqnum.to_be_bytes());
    out.extend([0u8; 12]); // devid, direction, ep: zero in replies
    out.extend(status.to_be_bytes());
    out.extend(actual_length.to_be_bytes());
    out.extend(start_frame.to_be_bytes());
    let npackets = if iso.is_empty() { 0 } else { iso.len() as u32 };
    out.extend(npackets.to_be_bytes());
    let errors = iso.iter().filter(|p| p.status != 0).count() as u32;
    out.extend(errors.to_be_bytes());
    out.extend([0u8; 8]);
    out.extend_from_slice(data);
    for p in iso {
        out.extend(p.offset.to_be_bytes());
        out.extend(p.length.to_be_bytes());
        out.extend(p.actual_length.to_be_bytes());
        out.extend(p.status.to_be_bytes());
    }
    out
}

pub fn ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HDR);
    out.extend(RET_UNLINK.to_be_bytes());
    out.extend(seqnum.to_be_bytes());
    out.extend([0u8; 12]);
    out.extend(status.to_be_bytes());
    out.extend([0u8; 24]);
    out
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &[u8] = include_bytes!("../../reference/device_descriptor.bin");
    const CONFIG: &[u8] = include_bytes!("../../reference/config_descriptor.bin");

    fn info() -> DeviceInfo {
        DeviceInfo::from_descriptors("1-1", DEVICE, CONFIG).unwrap()
    }

    #[test]
    fn device_info_from_reference_descriptors() {
        let d = info();
        assert_eq!((d.id_vendor, d.id_product, d.bcd_device), (0x0955, 0x7210, 0x0100));
        assert_eq!(d.num_configurations, 1);
        assert_eq!(d.interfaces, [(3, 0, 0), (1, 1, 0), (1, 2, 0), (1, 2, 0)]);
    }

    #[test]
    fn devlist_and_import_layout() {
        let d = info();
        let r = rep_devlist(&[&d]);
        assert_eq!(&r[..8], &[0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0]);
        assert_eq!(r.len(), 8 + 4 + 312 + 4 * 4);
        assert_eq!(&r[12 + 256..12 + 259], b"1-1");
        // idVendor right after path, busid, busnum, devnum, speed
        assert_eq!(&r[12 + 300..12 + 302], &[0x09, 0x55]);

        let r = rep_import(Some(&d));
        assert_eq!(r.len(), 8 + 312);
        assert_eq!(rep_import(None), [0x01, 0x11, 0x00, 0x03, 0, 0, 0, 1]);
    }

    #[test]
    fn parse_ops() {
        assert_eq!(parse(&[0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0], false), Ok(Some((Request::DevList, 8))));
        let mut imp = vec![0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0];
        assert_eq!(parse(&imp, false), Ok(None));
        imp.extend(b"1-1");
        imp.resize(8 + 32, 0);
        assert_eq!(parse(&imp, false), Ok(Some((Request::Import { busid: "1-1".into() }, 40))));
        assert_eq!(parse(&[0x01, 0x06, 0x80, 0x05, 0, 0, 0, 0], false), Err(ParseError::BadVersion(0x0106)));
    }

    #[expect(clippy::too_many_arguments, reason = "mirrors the header's fields one to one")]
    fn cmd(command: u32, seqnum: u32, dir: u32, ep: u32, len: u32, npackets: u32, extra: u32, setup: [u8; 8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [command, seqnum, 0x0001_0001, dir, ep, extra, len, 0, npackets, 0] {
            b.extend(v.to_be_bytes());
        }
        b.extend(setup);
        b
    }

    #[test]
    fn parse_control_in_submit() {
        let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
        let b = cmd(CMD_SUBMIT, 7, DIR_IN, 0, 18, 0, 0, setup);
        let (r, n) = parse(&b, true).unwrap().unwrap();
        assert_eq!(n, 48);
        let Request::Submit(s) = r else { panic!() };
        assert_eq!((s.seqnum, s.ep, s.direction, s.transfer_buffer_length), (7, 0, DIR_IN, 18));
        assert_eq!(s.setup, setup);
        assert!(s.data.is_empty() && s.iso.is_empty());
    }

    #[test]
    fn parse_out_submit_waits_for_payload() {
        let mut b = cmd(CMD_SUBMIT, 8, DIR_OUT, 0, 2, u32::MAX, 0, [0x21, 0x09, 0xfd, 0x03, 0, 0, 2, 0]);
        assert_eq!(parse(&b, true), Ok(None));
        b.extend([0xfd, 0x0a]);
        let (Request::Submit(s), 50) = parse(&b, true).unwrap().unwrap() else { panic!() };
        assert_eq!(s.data, [0xfd, 0x0a]);
        assert!(s.iso.is_empty(), "0xffffffff packets = not iso");
    }

    #[test]
    fn parse_iso_out_submit() {
        let mut b = cmd(CMD_SUBMIT, 9, DIR_OUT, 2, 256, 2, 0, [0; 8]);
        b.extend([0xAB; 256]);
        for (off, len) in [(0u32, 128u32), (128, 128)] {
            for v in [off, len, 0, 0] {
                b.extend(v.to_be_bytes());
            }
        }
        let (Request::Submit(s), n) = parse(&b, true).unwrap().unwrap() else { panic!() };
        assert_eq!(n, 48 + 256 + 32);
        assert_eq!(s.iso.len(), 2);
        assert_eq!(s.iso[1], IsoPacket { offset: 128, length: 128, actual_length: 0, status: 0 });
    }

    #[test]
    fn parse_unlink() {
        let b = cmd(CMD_UNLINK, 12, 0, 0, 0, 0, 9, [0; 8]);
        assert_eq!(parse(&b, true), Ok(Some((Request::Unlink { seqnum: 12, unlink_seqnum: 9 }, 48))));
    }

    #[test]
    fn rejects_oversized() {
        let b = cmd(CMD_SUBMIT, 1, DIR_OUT, 0, MAX_TRANSFER + 1, 0, 0, [0; 8]);
        assert!(matches!(parse(&b, true), Err(ParseError::TooLarge(_))));
    }

    #[test]
    fn ret_layouts() {
        let r = ret_submit(7, 0, &[1, 2, 3], 3, 0, &[]);
        assert_eq!(r.len(), 48 + 3);
        assert_eq!(&r[..8], &[0, 0, 0, 3, 0, 0, 0, 7]);
        assert_eq!(&r[24..28], &3u32.to_be_bytes());
        assert_eq!(&r[48..], &[1, 2, 3]);

        let r = ret_submit(9, -EPIPE, &[], 0, 0, &[]);
        assert_eq!(&r[20..24], &(-32i32).to_be_bytes());

        let iso = [IsoPacket { offset: 0, length: 68, actual_length: 32, status: 0 }];
        let r = ret_submit(10, 0, &[0; 32], 32, 5, &iso);
        assert_eq!(r.len(), 48 + 32 + 16);
        assert_eq!(&r[28..32], &5i32.to_be_bytes(), "start_frame");
        assert_eq!(&r[32..36], &1u32.to_be_bytes(), "number_of_packets");
        assert_eq!(&r[36..40], &0u32.to_be_bytes(), "error_count");

        let r = ret_unlink(12, -ECONNRESET);
        assert_eq!(r.len(), 48);
        assert_eq!(&r[..4], &4u32.to_be_bytes());
        assert_eq!(&r[20..24], &(-104i32).to_be_bytes());
    }
}
