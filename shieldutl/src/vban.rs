//! Controller audio over VBAN, Voicemeeter's UDP audio protocol, toggled from the tray. The
//! mic goes out as stream ShieldMic; stream ShieldPad comes in for the headset jack.
//! Spec: https://vb-audio.com/Voicemeeter/VBANProtocol_Specifications.pdf
//!
//! Ported from linux/shield_audio.py, which did the same from WSL. The headset side carries
//! its own resampler and jitter buffer, since there's no ALSA here to lean on.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

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

//region Mic

const MIC_RATE: u32 = 16000;
/// 10 ms per packet
const MIC_SAMPLES: usize = 160;
/// Auto gain: aim for ~-18 dBFS RMS, never more than +24 dB, and leave near-silence alone
/// rather than pump up the hiss
const AGC_TARGET: f32 = 32768.0 * 0.125_892_54;
const AGC_MAX_GAIN: f32 = 15.848_932;
const AGC_GATE: f32 = 32768.0 * 0.001_778_279_4;
/// Per 10 ms: cut fast, raise slowly
const AGC_ATTACK: f32 = 0.5;
const AGC_RELEASE: f32 = 0.02;

struct Mic {
    sock: UdpSocket,
    samples: Vec<i16>,
    gain: f32,
    frame: u32,
}

impl Mic {
    fn push(&mut self, pcm: &[u8]) {
        self.samples.extend(pcm.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b)));
        while self.samples.len() >= MIC_SAMPLES {
            let chunk: Vec<i16> = self.samples.drain(..MIC_SAMPLES).collect();
            self.send(&chunk);
        }
    }

    fn send(&mut self, mono: &[i16]) {
        let rms = (mono.iter().map(|&x| f32::from(x).powi(2)).sum::<f32>() / mono.len() as f32).sqrt();
        if rms > AGC_GATE {
            let want = (AGC_TARGET / rms).clamp(1.0, AGC_MAX_GAIN);
            let rate = if want < self.gain { AGC_ATTACK } else { AGC_RELEASE };
            self.gain += (want - self.gain) * rate;
        }
        let mut p = header(MIC_RATE, MIC_SAMPLES, 2, MIC_STREAM, self.frame);
        // Mono doubled to L+R, which Voicemeeter's strips expect
        for &x in mono {
            let s = (f32::from(x) * self.gain).clamp(-32768.0, 32767.0) as i16;
            p.extend_from_slice(&s.to_le_bytes());
            p.extend_from_slice(&s.to_le_bytes());
        }
        self.frame = self.frame.wrapping_add(1);
        if let Err(e) = self.sock.send_to(&p, (Ipv4Addr::LOCALHOST, MIC_PORT)) {
            debug!("mic VBAN send: {e}");
        }
    }
}

//endregion

//region Headset

/// What the headset plays: 32 kHz stereo s16le, per its audio streaming descriptors
const PAD_RATE: u32 = 32000;
/// The controller's isoc unit: 4 ms (ms_per_isoc), 32 frames each
pub const UNIT_MS: u8 = 4;
const UNIT_FRAMES: usize = 32 * UNIT_MS as usize;
/// Start playing once this much is buffered, and again after running dry
const START_FRAMES: usize = 32 * 60;
/// More than this and the oldest goes, to keep latency from creeping up; Voicemeeter's clock
/// and ours never quite agree
const MAX_FRAMES: usize = 32 * 150;
/// The headset jack has no volume control of its own; full scale is very loud
const PAD_GAIN: f32 = 0.251_188_64;

/// Linear resampler to PAD_RATE, any rate in
#[derive(Default)]
struct Resampler {
    rate: u32,
    /// Position of the next output frame past `prev`, in input frames
    t: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn push(&mut self, rate: u32, frame: [f32; 2], out: &mut VecDeque<[i16; 2]>) {
        if rate != self.rate {
            *self = Resampler { rate, t: 0.0, prev: frame };
        }
        let step = f64::from(rate) / f64::from(PAD_RATE);
        while self.t <= 1.0 {
            let t = self.t as f32;
            let s = |c: usize| (self.prev[c] + (frame[c] - self.prev[c]) * t) * PAD_GAIN;
            out.push_back([s(0) as i16, s(1) as i16]);
            self.t += step;
        }
        self.t -= 1.0;
        self.prev = frame;
    }
}

#[derive(Default)]
struct PadBuf {
    frames: VecDeque<[i16; 2]>,
    playing: bool,
    trimmed: usize,
    /// Times it ran dry while playing
    dry: usize,
}

impl PadBuf {
    fn take_unit(&mut self) -> Option<Vec<u8>> {
        if !self.playing && self.frames.len() >= START_FRAMES {
            self.playing = true;
        }
        if !self.playing {
            return None;
        }
        if self.frames.len() < UNIT_FRAMES {
            self.playing = false;
            self.dry += 1;
            return None;
        }
        Some(self.frames.drain(..UNIT_FRAMES).flat_map(|[l, r]| [l.to_le_bytes(), r.to_le_bytes()]).flatten().collect())
    }

    fn trim(&mut self) {
        if self.frames.len() > MAX_FRAMES {
            let n = self.frames.len() - START_FRAMES;
            self.frames.drain(..n);
            self.trimmed += n;
        }
    }
}

fn receive(sock: UdpSocket, pad: Arc<Mutex<PadBuf>>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    let mut rs = Resampler::default();
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
        let mut p = pad.lock().unwrap_or_else(|e| e.into_inner());
        for f in data.chunks_exact(h.channels * 2) {
            let s = |c: usize| f32::from(i16::from_le_bytes([f[c * 2], f[c * 2 + 1]]));
            let frame = if h.channels == 1 { [s(0); 2] } else { [s(0), s(1)] };
            rs.push(h.rate, frame, &mut p.frames);
        }
        p.trim();
    }
}

//endregion

/// Both streams, while VBAN is on
pub struct Vban {
    mic: Mic,
    pad: Arc<Mutex<PadBuf>>,
    stop: Arc<AtomicBool>,
    rx: Option<JoinHandle<()>>,
}

impl Vban {
    pub fn start() -> Result<Vban> {
        let mic = Mic {
            sock: UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).context("mic VBAN socket")?,
            samples: Vec::new(),
            gain: 1.0,
            frame: 0,
        };
        let sock = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PAD_PORT)))
            .with_context(|| format!("listening for VBAN on port {PAD_PORT}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let pad = Arc::new(Mutex::new(PadBuf::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let rx = {
            let (pad, stop) = (pad.clone(), stop.clone());
            std::thread::Builder::new().name("vban".into()).spawn(move || receive(sock, pad, stop))?
        };
        info!(mic = MIC_STREAM, mic_port = MIC_PORT, headset = PAD_STREAM, headset_port = PAD_PORT, "VBAN on");
        Ok(Vban { mic, pad, stop, rx: Some(rx) })
    }

    /// Mic samples from the controller, 16 kHz mono s16le
    pub fn push_mic(&mut self, pcm: &[u8]) {
        self.mic.push(pcm);
    }

    /// One isoc unit of headset audio, UNIT_MS of 32 kHz stereo s16le, if there's enough
    pub fn take_unit(&self) -> Option<Vec<u8>> {
        let mut p = self.pad.lock().unwrap_or_else(|e| e.into_inner());
        if p.trimmed > 0 {
            debug!(frames = p.trimmed, "headset audio running ahead, dropped some");
            p.trimmed = 0;
        }
        if p.dry > 0 {
            debug!(times = p.dry, "headset audio ran dry, waiting to refill");
            p.dry = 0;
        }
        p.take_unit()
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

    #[test]
    fn resamples_to_32k() {
        let mut out = VecDeque::new();
        let mut rs = Resampler::default();
        for i in 0..4800 {
            rs.push(48000, [i as f32; 2], &mut out);
        }
        // 100 ms in, 100 ms out, give or take the last frame
        assert!((3199..=3201).contains(&out.len()), "{}", out.len());
        // A ramp stays a ramp, attenuated
        let (a, b) = (out[100][0], out[200][0]);
        assert!(b > a);
    }

    #[test]
    fn buffer_starts_late_and_stops_dry() {
        let mut p = PadBuf::default();
        p.frames.extend(std::iter::repeat_n([0i16; 2], START_FRAMES - 1));
        assert!(p.take_unit().is_none());
        p.frames.push_back([0; 2]);
        assert_eq!(p.take_unit().map(|u| u.len()), Some(UNIT_FRAMES * 4));
        p.frames.truncate(UNIT_FRAMES - 1);
        assert!(p.take_unit().is_none());
        assert!(!p.playing);
        p.frames.extend(std::iter::repeat_n([0i16; 2], MAX_FRAMES * 2));
        p.trim();
        assert_eq!(p.frames.len(), START_FRAMES);
    }
}
