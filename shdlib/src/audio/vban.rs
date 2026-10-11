//! Controller audio over VBAN, Voicemeeter's UDP audio protocol: the mics go out mixed as stream
//! ShieldMic, and stream ShieldPad comes in for every headset jack.
//! Spec: https://vb-audio.com/Voicemeeter/VBANProtocol_Specifications.pdf
//!
//! Ported from linux/shield_audio.py, which did the same from WSL. Windows' and WSL's backend;
//! Linux uses it with `--audio-backend=vban`, or when PipeWire isn't running.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::{Headset, Jitter, MIC_RATE, MIC_SAMPLES};

pub const MIC_STREAM: &str = "ShieldMic";
pub const PAD_STREAM: &str = "ShieldPad";
/// Voicemeeter's VBAN listen port
pub const MIC_PORT: u16 = 6980;
/// Not 6980: in mirrored mode that's Voicemeeter's own
pub const PAD_PORT: u16 = 6981;

//region Wire format

/// 'VBAN', sr | proto, samples - 1, channels - 1, format | codec, name[16], frame number
const HEADER_LEN: usize = 28;
const SR_LIST: [u32; 21] = [
    6000, 12000, 24000, 48000, 96000, 192000, 384000, 8000, 16000, 32000, 64000, 128000, 256000, 512000, 11025, 22050,
    44100, 88200, 176400, 352800, 705600,
];
const PROTO_AUDIO: u8 = 0x00;
/// Low 3 bits: sample type; high bits: codec, 0 = PCM
const FMT_INT16: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    rate: u32,
    samples: usize,
    channels: usize,
    frame: u32,
}

fn header(rate: u32, samples: usize, channels: usize, name: &str, frame: u32) -> Vec<u8> {
    let sr = SR_LIST.iter().position(|&r| r == rate).expect("a VBAN rate") as u8;
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(b"VBAN");
    h.extend_from_slice(&[sr | PROTO_AUDIO, (samples - 1) as u8, (channels - 1) as u8, FMT_INT16]);
    let mut n = [0u8; 16];
    n[..name.len()].copy_from_slice(name.as_bytes());
    h.extend_from_slice(&n);
    h.extend_from_slice(&frame.to_le_bytes());
    h
}

/// The header of a PCM16 audio packet named `stream`, and its samples. Err for audio in a
/// format we can't play, so it can be reported.
fn parse<'a>(p: &'a [u8], stream: &str) -> Option<Result<(Header, &'a [u8]), String>> {
    if p.len() <= HEADER_LEN || &p[..4] != b"VBAN" || p[4] & 0xE0 != PROTO_AUDIO {
        return None;
    }
    let name = &p[8..24];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(16)];
    if !name.eq_ignore_ascii_case(stream.as_bytes()) {
        return None;
    }
    let Some(&rate) = SR_LIST.get(usize::from(p[4] & 0x1F)) else {
        return Some(Err("unknown sample rate".into()));
    };
    if p[7] != FMT_INT16 {
        return Some(Err("not PCM 16-bit; set the stream to PCM16 in Voicemeeter".into()));
    }
    let h = Header {
        rate,
        samples: usize::from(p[5]) + 1,
        channels: usize::from(p[6]) + 1,
        frame: u32::from_le_bytes(p[24..28].try_into().unwrap()),
    };
    let data = p.get(HEADER_LEN..HEADER_LEN + h.samples * h.channels * 2)?;
    Some(Ok((h, data)))
}

//endregion

fn receive(sock: UdpSocket, pad: Arc<Mutex<Jitter<[i16; 2]>>>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    let mut headset = Headset::default();
    let mut last: Option<(Header, u32)> = None;
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        let Ok(n) = sock.recv(&mut buf) else { continue };
        let (h, data) = match parse(&buf[..n], PAD_STREAM) {
            Some(Ok(p)) => p,
            Some(Err(why)) => {
                if !warned {
                    warn!(stream = PAD_STREAM, "ignoring VBAN stream: {why}");
                    warned = true;
                }
                continue;
            }
            None => continue,
        };
        // A frame number a little behind the last is a reordered straggler; far behind is a
        // restarted stream
        if let Some((_, f)) = last
            && (1..=64).contains(&f.wrapping_sub(h.frame))
        {
            continue;
        }
        if last.is_none_or(|(l, _)| (l.rate, l.channels) != (h.rate, h.channels)) {
            info!(stream = PAD_STREAM, rate = h.rate, channels = h.channels, "headset stream");
        }
        last = Some((h, h.frame));
        headset.push(&pad, h.rate, h.channels, data);
    }
}

/// Both streams: the mic's socket, and the headset's on its own thread
pub(super) struct Vban {
    sock: UdpSocket,
    frame: u32,
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
}

impl Vban {
    /// Headset audio goes onto `pad`
    pub(super) fn start(pad: Arc<Mutex<Jitter<[i16; 2]>>>) -> Result<Vban> {
        let mic = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).context("mic VBAN socket")?;
        let sock = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PAD_PORT)))
            .with_context(|| format!("listening for VBAN on port {PAD_PORT}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let rx = {
            let stop = stop.clone();
            std::thread::Builder::new().name("vban".into()).spawn(move || receive(sock, pad, stop))?
        };
        info!(mic = MIC_STREAM, mic_port = MIC_PORT, headset = PAD_STREAM, headset_port = PAD_PORT, "VBAN on");
        Ok(Vban { sock: mic, frame: 0, stop, rx: Some(rx) })
    }

    /// One packet of the mix
    pub(super) fn send_mic(&mut self, mono: &[i16]) {
        let mut p = header(MIC_RATE, MIC_SAMPLES, 2, MIC_STREAM, self.frame);
        // Mono doubled to L+R, which Voicemeeter's strips expect
        for &x in mono {
            p.extend_from_slice(&x.to_le_bytes());
            p.extend_from_slice(&x.to_le_bytes());
        }
        self.frame = self.frame.wrapping_add(1);
        if let Err(e) = self.sock.send_to(&p, (Ipv4Addr::LOCALHOST, MIC_PORT)) {
            debug!("mic VBAN send: {e}");
        }
    }
}

impl Drop for Vban {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.rx.take() {
            let _ = t.join();
        }
        info!("VBAN off");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let mut p = header(48000, 256, 2, PAD_STREAM, 7);
        p.extend(std::iter::repeat_n(0u8, 256 * 2 * 2));
        let (h, data) = parse(&p, "shieldpad").unwrap().unwrap();
        assert_eq!(h, Header { rate: 48000, samples: 256, channels: 2, frame: 7 });
        assert_eq!(data.len(), 1024);
        assert!(parse(&p, MIC_STREAM).is_none());
        // Same header as shield_audio.py's: 16 kHz is index 8
        assert_eq!(header(MIC_RATE, MIC_SAMPLES, 2, MIC_STREAM, 0)[4..8], [8, 159, 1, 1]);
    }
}
