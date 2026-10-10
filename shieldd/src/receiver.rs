//! `--receiver`: the radio stays in WSL (ozwpan + `shieldd --transmit`, or the
//! older shield_tx.py) and shieldd only drives the outputs, taking the place of
//! `shield_rx.py`. Wire format: the `SHD2` packet in the `shdlink` crate.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shdlink::media;
use shdnet::mac::Mac;
use tracing::{debug, info, trace, warn};
use vigem_client::{XButtons, XGamepad};

use crate::controller::key;
use crate::outputs::{Desktop, Gamepad};

/// No packets this long: let go of everything. Same as shield_rx.py.
const STALE: Duration = Duration::from_millis(500);

/// SHD2 media bits -> controller key bits, which `Desktop` speaks
const MEDIA: [(u8, u8); 3] = [(media::VOL_UP, key::VOL_UP), (media::VOL_DOWN, key::VOL_DOWN), (media::MUTE, key::MUTE)];

#[derive(Debug, Clone, Copy, PartialEq)]
struct Packet {
    seq: u32,
    pad: XGamepad,
    /// Controller key bits
    keys: u8,
    /// Bit 0 = left, 1 = right, 2 = middle, 3 = X1
    mouse_buttons: u8,
    dx: i16,
    dy: i16,
}

impl Packet {
    fn parse(b: &[u8]) -> Option<Packet> {
        let p = shdlink::Packet::parse(b)?;
        Some(Packet {
            seq: p.seq,
            pad: XGamepad {
                buttons: XButtons { raw: p.buttons },
                thumb_lx: stick(p.lx),
                thumb_ly: stick(p.ly),
                thumb_rx: stick(p.rx),
                thumb_ry: stick(p.ry),
                left_trigger: trigger(p.lt),
                right_trigger: trigger(p.rt),
            },
            keys: MEDIA.iter().filter(|(bit, _)| p.media & bit != 0).fold(0, |k, (_, to)| k | to),
            mouse_buttons: p.mouse_buttons,
            dx: p.dx,
            dy: p.dy,
        })
    }
}

/// Same scaling as vgamepad, which shield_rx.py used
fn stick(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn trigger(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The one transmitter, as Desktop's source
const TRANSMITTER: Mac = [0; 6];

/// Runs until ctrl+c.
pub fn run(listen: SocketAddr, mouse_scale: (f32, f32), quit: &AtomicBool) -> Result<()> {
    let mut gamepad = Gamepad::new();
    let mut desktop = Desktop::new(mouse_scale);
    let sock = UdpSocket::bind(listen).with_context(|| format!("binding {listen}; is shield_rx.py still running?"))?;
    sock.set_read_timeout(Some(STALE))?;
    info!(%listen, "receiver mode: waiting for shieldd --transmit (or shield_tx.py). ctrl+c to stop");

    let mut last_seq: Option<u32> = None;
    let mut buf = [0u8; 64];
    while !quit.load(Ordering::Relaxed) {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if last_seq.take().is_some() {
                    debug!("no packets for {STALE:?}, releasing outputs");
                    gamepad.release();
                    desktop.release(&TRANSMITTER);
                }
                continue;
            }
            // A port-unreachable from an earlier send; UDP keeps going
            Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e).context("receiving"),
        };
        let Some(p) = Packet::parse(&buf[..n]) else {
            continue;
        };
        trace!(?p, "packet");
        // Drop reordered stragglers
        if let Some(last) = last_seq
            && p.seq.wrapping_sub(last) > 0x7FFF_FFFF
        {
            continue;
        }
        if last_seq.is_none() {
            info!("transmitter connected");
            if let Err(e) = gamepad.plug() {
                warn!("{e:#}");
            }
        }
        last_seq = Some(p.seq);
        gamepad.update(&p.pad);
        desktop.set_keys(&TRANSMITTER, p.keys);
        desktop.move_counts(&TRANSMITTER, f32::from(p.dx), f32::from(p.dy));
        desktop.set_mouse_buttons(&TRANSMITTER, p.mouse_buttons);
        // Repeats only as often as packets come in
        desktop.poll(Instant::now());
        trace!(seq = p.seq, "applied");
    }
    info!("shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shdlink::{MAGIC, PACKET_LEN};

    /// struct.pack("<4sIHffffffBBhh", ...) as shield_tx.py builds it
    fn pack(seq: u32, buttons: u16, axes: [f32; 6], media: u8, mb: u8, dx: i16, dy: i16) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.extend(seq.to_le_bytes());
        b.extend(buttons.to_le_bytes());
        for a in axes {
            b.extend(a.to_le_bytes());
        }
        b.extend([media, mb]);
        b.extend(dx.to_le_bytes());
        b.extend(dy.to_le_bytes());
        b
    }

    #[test]
    fn parses_shield_tx_packet() {
        let b = pack(7, XButtons::A | XButtons::GUIDE, [-1.0, 1.0, 0.5, 0.0, 1.0, 0.0], 0x05, 0x09, -3, 12);
        assert_eq!(b.len(), PACKET_LEN);
        let p = Packet::parse(&b).unwrap();
        assert_eq!(p.seq, 7);
        assert_eq!(p.pad.buttons.raw, XButtons::A | XButtons::GUIDE);
        assert_eq!((p.pad.thumb_lx, p.pad.thumb_ly, p.pad.thumb_rx, p.pad.thumb_ry), (-32767, 32767, 16384, 0));
        assert_eq!((p.pad.left_trigger, p.pad.right_trigger), (255, 0));
        assert_eq!(p.keys, key::VOL_UP | key::MUTE);
        assert_eq!((p.mouse_buttons, p.dx, p.dy), (0x09, -3, 12));
    }

    #[test]
    fn rejects_foreign_packets() {
        let mut b = pack(1, 0, [0.0; 6], 0, 0, 0, 0);
        assert!(Packet::parse(&b[..39]).is_none());
        b[3] = b'1';
        assert!(Packet::parse(&b).is_none());
    }

    #[test]
    fn out_of_range_axes_clamp() {
        let p = Packet::parse(&pack(1, 0, [2.0, -2.0, 0.0, 0.0, -1.0, 9.0], 0, 0, 0, 0)).unwrap();
        assert_eq!((p.pad.thumb_lx, p.pad.thumb_ly), (32767, -32767));
        assert_eq!((p.pad.left_trigger, p.pad.right_trigger), (0, 255));
    }
}
