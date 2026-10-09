//! The `SHD2` packet: XInput-shaped controller state, one per input frame plus
//! a keepalive, sent over UDP from WSL (`shieldd --transmit`, formerly
//! shield_tx.py) to Windows (`shieldd --receiver`, formerly shield_rx.py).
//!
//! Wire format, little-endian, 40 bytes (`struct.pack("<4sIHffffffBBhh")`):
//! magic, seq, XInput buttons, lx ly rx ry in -1..1, lt rt in 0..1,
//! media keys, mouse buttons, trackpad dx dy in counts.

/// Default address both sides use; in WSL mirrored mode it's Windows' localhost
pub const DEFAULT_ADDR: &str = "127.0.0.1:26760";
pub const MAGIC: &[u8; 4] = b"SHD2";
pub const PACKET_LEN: usize = 40;

/// XUSB button bits (what ViGEm expects)
pub mod xb {
    pub const UP: u16 = 0x0001;
    pub const DOWN: u16 = 0x0002;
    pub const LEFT: u16 = 0x0004;
    pub const RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LTHUMB: u16 = 0x0040;
    pub const RTHUMB: u16 = 0x0080;
    pub const LB: u16 = 0x0100;
    pub const RB: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Media key bits
pub mod media {
    pub const VOL_UP: u8 = 0x01;
    pub const VOL_DOWN: u8 = 0x02;
    pub const MUTE: u8 = 0x04;
}

/// Mouse button bits
pub mod mouse {
    pub const LEFT: u8 = 0x01;
    pub const RIGHT: u8 = 0x02;
    pub const MIDDLE: u8 = 0x04;
    pub const X1: u8 = 0x08;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Packet {
    pub seq: u32,
    /// `xb` bits
    pub buttons: u16,
    /// Sticks in -1..1, up positive
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    /// Triggers in 0..1
    pub lt: f32,
    pub rt: f32,
    /// `media` bits
    pub media: u8,
    /// `mouse` bits
    pub mouse_buttons: u8,
    /// Trackpad motion since the previous packet, in pad counts
    pub dx: i16,
    pub dy: i16,
}

impl Packet {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..10].copy_from_slice(&self.buttons.to_le_bytes());
        for (i, v) in [self.lx, self.ly, self.rx, self.ry, self.lt, self.rt].into_iter().enumerate() {
            b[10 + i * 4..14 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[34] = self.media;
        b[35] = self.mouse_buttons;
        b[36..38].copy_from_slice(&self.dx.to_le_bytes());
        b[38..40].copy_from_slice(&self.dy.to_le_bytes());
        b
    }

    /// None for anything that isn't exactly one SHD2 packet (an old or foreign sender)
    pub fn parse(b: &[u8]) -> Option<Packet> {
        if b.len() != PACKET_LEN || &b[..4] != MAGIC {
            return None;
        }
        let f = |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Some(Packet {
            seq: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            buttons: u16::from_le_bytes([b[8], b[9]]),
            lx: f(10),
            ly: f(14),
            rx: f(18),
            ry: f(22),
            lt: f(26),
            rt: f(30),
            media: b[34],
            mouse_buttons: b[35],
            dx: i16::from_le_bytes([b[36], b[37]]),
            dy: i16::from_le_bytes([b[38], b[39]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let p = Packet {
            seq: 0xDEAD_BEEF,
            buttons: xb::A | xb::GUIDE,
            lx: -1.0,
            ly: 0.5,
            rx: 0.25,
            ry: -0.75,
            lt: 1.0,
            rt: 0.0,
            media: media::VOL_UP | media::MUTE,
            mouse_buttons: mouse::LEFT | mouse::X1,
            dx: -300,
            dy: 12,
        };
        assert_eq!(Packet::parse(&p.encode()), Some(p));
    }

    /// Byte layout as shield_tx.py's struct.pack("<4sIHffffffBBhh") built it
    #[test]
    fn matches_python_layout() {
        let p = Packet { seq: 7, buttons: 0x1400, lx: 1.0, media: 0x05, mouse_buttons: 0x09, dx: -3, dy: 12, ..Default::default() };
        let b = p.encode();
        assert_eq!(&b[..4], b"SHD2");
        assert_eq!(&b[4..8], &[7, 0, 0, 0]);
        assert_eq!(&b[8..10], &[0x00, 0x14]);
        assert_eq!(&b[10..14], &1.0f32.to_le_bytes());
        assert_eq!(&b[34..], &[0x05, 0x09, 0xfd, 0xff, 12, 0]);
    }

    #[test]
    fn rejects_foreign_packets() {
        let mut b = Packet::default().encode();
        assert!(Packet::parse(&b[..39]).is_none());
        b[3] = b'1';
        assert!(Packet::parse(&b).is_none());
    }
}
