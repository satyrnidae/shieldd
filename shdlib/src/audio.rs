//! Controller audio, toggled per controller from the tray. Every controller with audio on shares
//! one mic and one headset: their mics are mixed into one, and each headset plays the same audio.
//! Where that goes is the backend's: VBAN streams for Voicemeeter (vban.rs), or a mic and a
//! headset of shieldd's own in PipeWire (pipewire.rs). What's left is theirs alike: the mixer
//! with its auto gain, and the headset's resampler and jitter buffer.

#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pipewire;
pub mod vban;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::debug;

use shdnet::mac::Mac;

/// Where controller audio goes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// VBAN streams on localhost, for Voicemeeter
    #[default]
    Vban,
    /// A mic and a headset in the desktop's sound settings, MIC_NAME and HEADSET_NAME. Linux only.
    PipeWire,
}

impl Backend {
    /// PipeWire where it's running, VBAN otherwise: Windows, WSL, or Linux without a sound server
    pub fn detect() -> Backend {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if pipewire::running() {
            return Backend::PipeWire;
        }
        Backend::Vban
    }
}

/// What the desktop calls PipeWire's mic and headset
pub const MIC_NAME: &str = "SHIELD controller mic";
pub const HEADSET_NAME: &str = "SHIELD controller headset";

//region Mic

/// What the controller's mic sends: 16 kHz mono s16le
const MIC_RATE: u32 = 16000;
/// The mix goes out 10 ms at a time
const MIC_SAMPLES: usize = 160;
/// Auto gain: aim for ~-18 dBFS RMS, never more than +24 dB, and leave near-silence alone
/// rather than pump up the hiss
const AGC_TARGET: f32 = 32768.0 * 0.125_892_54;
const AGC_MAX_GAIN: f32 = 15.848_932;
const AGC_GATE: f32 = 32768.0 * 0.001_778_279_4;
/// Per 10 ms: cut fast, raise slowly
const AGC_ATTACK: f32 = 0.5;
const AGC_RELEASE: f32 = 0.02;
/// A mic that's sent nothing this long isn't waited for: its controller dozed or left
const MIX_GRACE: Duration = Duration::from_millis(80);
/// Most a mic can run ahead of the others before its oldest samples go: the grace's worth, as
/// no two clocks agree
const MIX_MAX: usize = MIC_SAMPLES * 8;
/// Most packets one push sends. A backlog freed by a mic going quiet goes, oldest first, rather
/// than out in one burst that Voicemeeter's jitter buffer chokes on.
const MIX_BURST: usize = 2;

/// Each controller's mic samples, until every mic still sending has a packet's worth. Those
/// are summed, saturating, into one.
#[derive(Default)]
struct Mixer {
    /// Samples waiting, and when that mic last sent any
    mics: HashMap<Mac, (VecDeque<i16>, Instant)>,
}

impl Mixer {
    /// Adds `src`'s samples; returns the packets' worth now mixed
    fn push(&mut self, src: Mac, pcm: &[u8], now: Instant) -> Vec<Vec<i16>> {
        let (q, last) = self.mics.entry(src).or_insert_with(|| (VecDeque::new(), now));
        q.extend(pcm.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b)));
        *last = now;
        if q.len() > MIX_MAX {
            let n = q.len() - MIX_MAX;
            q.drain(..n);
        }
        let quiet = |last: &Instant| now.duration_since(*last) >= MIX_GRACE;
        let mut out = Vec::new();
        while self.mics.values().any(|(q, _)| q.len() >= MIC_SAMPLES)
            && self.mics.values().all(|(q, last)| q.len() >= MIC_SAMPLES || quiet(last))
        {
            let mut mix = [0i32; MIC_SAMPLES];
            for (q, _) in self.mics.values_mut() {
                let n = q.len().min(MIC_SAMPLES);
                for (m, x) in mix.iter_mut().zip(q.drain(..n)) {
                    *m += i32::from(x);
                }
            }
            out.push(mix.iter().map(|&x| x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16).collect());
        }
        self.mics.retain(|_, (q, last)| !q.is_empty() || !quiet(last));
        if out.len() > MIX_BURST {
            out.drain(..out.len() - MIX_BURST);
        }
        out
    }
}

/// The controller's mic is quiet; this brings the mix up to a level apps expect
struct Agc {
    gain: f32,
}

impl Default for Agc {
    fn default() -> Self {
        Agc { gain: 1.0 }
    }
}

impl Agc {
    /// One packet of the mix, MIC_SAMPLES long
    fn apply(&mut self, mono: &mut [i16]) {
        let rms = (mono.iter().map(|&x| f32::from(x).powi(2)).sum::<f32>() / mono.len() as f32).sqrt();
        if rms > AGC_GATE {
            let want = (AGC_TARGET / rms).clamp(1.0, AGC_MAX_GAIN);
            let rate = if want < self.gain { AGC_ATTACK } else { AGC_RELEASE };
            self.gain += (want - self.gain) * rate;
        }
        for x in mono {
            *x = (f32::from(*x) * self.gain).clamp(-32768.0, 32767.0) as i16;
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
/// More than this and the oldest goes, to keep latency from creeping up; the backend's clock
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

/// Headset audio as the backend has it, onto the jitter buffer
#[derive(Default)]
struct Headset {
    rs: Resampler,
}

impl Headset {
    /// s16le at `rate`, mono or stereo
    fn push(&mut self, pad: &Mutex<Jitter<[i16; 2]>>, rate: u32, channels: usize, pcm: &[u8]) {
        let mut p = lock(pad);
        for f in pcm.chunks_exact(channels * 2) {
            let s = |c: usize| f32::from(i16::from_le_bytes([f[c * 2], f[c * 2 + 1]]));
            let frame = if channels == 1 { [s(0); 2] } else { [s(0), s(1)] };
            self.rs.push(rate, frame, &mut p.frames);
        }
        p.trim();
    }
}

//endregion

/// Audio between two clocks that never quite agree: taken out only once there's `start` frames
/// in, and trimmed back to that past `max`
struct Jitter<F> {
    frames: VecDeque<F>,
    start: usize,
    max: usize,
    playing: bool,
    trimmed: usize,
    /// Times it ran dry while playing
    dry: usize,
}

impl<F> Jitter<F> {
    fn new(start: usize, max: usize) -> Self {
        Jitter { frames: VecDeque::new(), start, max, playing: false, trimmed: 0, dry: 0 }
    }

    /// The next `n` frames, if there's enough
    fn take(&mut self, n: usize) -> Option<std::collections::vec_deque::Drain<'_, F>> {
        if !self.playing && self.frames.len() >= self.start.max(n) {
            self.playing = true;
        }
        if !self.playing {
            return None;
        }
        if self.frames.len() < n {
            self.playing = false;
            self.dry += 1;
            return None;
        }
        Some(self.frames.drain(..n))
    }

    fn trim(&mut self) {
        if self.frames.len() > self.max {
            let n = self.frames.len() - self.start;
            self.frames.drain(..n);
            self.trimmed += n;
        }
    }

    /// Logs the trims and dry spells since last time
    fn report(&mut self, what: &str) {
        if self.trimmed > 0 {
            debug!(frames = self.trimmed, "{what} audio running ahead, dropped some");
            self.trimmed = 0;
        }
        if self.dry > 0 {
            debug!(times = self.dry, "{what} audio ran dry, waiting to refill");
            self.dry = 0;
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

enum Out {
    Vban(vban::Vban),
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    PipeWire(pipewire::PipeWire),
}

/// The shared mic and headset, while audio is on for any controller
pub struct Audio {
    out: Out,
    mixer: Mixer,
    agc: Agc,
    pad: Arc<Mutex<Jitter<[i16; 2]>>>,
}

impl Audio {
    pub fn start(backend: Backend) -> Result<Audio> {
        let pad = Arc::new(Mutex::new(Jitter::new(START_FRAMES, MAX_FRAMES)));
        let out = match backend {
            Backend::Vban => Out::Vban(vban::Vban::start(pad.clone())?),
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Backend::PipeWire => Out::PipeWire(pipewire::PipeWire::start(pad.clone())?),
            #[cfg(not(all(target_os = "linux", feature = "pipewire")))]
            Backend::PipeWire => anyhow::bail!("this build has no PipeWire"),
        };
        Ok(Audio { out, mixer: Mixer::default(), agc: Agc::default(), pad })
    }

    pub fn backend(&self) -> Backend {
        match self.out {
            Out::Vban(_) => Backend::Vban,
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Out::PipeWire(_) => Backend::PipeWire,
        }
    }

    /// Mic samples from controller `src`, 16 kHz mono s16le, mixed with the others'
    pub fn push_mic(&mut self, src: Mac, pcm: &[u8], now: Instant) {
        for mut mono in self.mixer.push(src, pcm, now) {
            self.agc.apply(&mut mono);
            match &mut self.out {
                Out::Vban(v) => v.send_mic(&mono),
                #[cfg(all(target_os = "linux", feature = "pipewire"))]
                Out::PipeWire(p) => p.push_mic(&mono),
            }
        }
    }

    /// Drops what `src`'s mic had waiting; for when audio goes off for it
    pub fn forget_mic(&mut self, src: &Mac) {
        self.mixer.mics.remove(src);
    }

    /// One isoc unit of headset audio, UNIT_MS of 32 kHz stereo s16le, if there's enough. The same
    /// unit goes to every controller with audio on.
    pub fn take_unit(&self) -> Option<Vec<u8>> {
        let mut p = lock(&self.pad);
        p.report("headset");
        Some(p.take(UNIT_FRAMES)?.flat_map(|[l, r]| [l.to_le_bytes(), r.to_le_bytes()]).flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(v: i16, n: usize) -> Vec<u8> {
        std::iter::repeat_n(v.to_le_bytes(), n).flatten().collect()
    }

    #[test]
    fn mics_mix_saturating() {
        let (a, b) = ([1; 6], [2; 6]);
        let t = Instant::now();
        let mut m = Mixer::default();
        // Alone, a mic goes straight out
        assert_eq!(m.push(a, &pcm(1000, MIC_SAMPLES), t), vec![vec![1000; MIC_SAMPLES]]);
        // Two wait for each other, then sum
        let mut m = Mixer::default();
        assert_eq!(m.push(a, &pcm(1, 10), t), Vec::<Vec<i16>>::new());
        assert!(m.push(b, &pcm(1, 10), t).is_empty());
        assert!(m.push(a, &pcm(30000, MIC_SAMPLES), t).is_empty(), "b's not caught up");
        let out = m.push(b, &pcm(30000, MIC_SAMPLES), t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][..10], [2; 10]);
        assert_eq!(out[0][10..], [i16::MAX; MIC_SAMPLES - 10]);
    }

    #[test]
    fn a_quiet_mic_doesnt_hold_up_the_mix() {
        let (a, b) = ([1; 6], [2; 6]);
        let t = Instant::now();
        let mut m = Mixer::default();
        assert!(m.push(b, &pcm(5, 4), t).is_empty());
        assert!(m.push(a, &pcm(7, MIC_SAMPLES), t).is_empty());
        // b went quiet: its leftovers go in the next packet, then it's dropped
        let out = m.push(a, &pcm(7, MIC_SAMPLES), t + MIX_GRACE);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][..4], [12; 4]);
        assert_eq!(out[0][4..], [7; MIC_SAMPLES - 4]);
        assert_eq!(out[1], vec![7; MIC_SAMPLES]);
        assert!(!m.mics.contains_key(&b));
    }

    #[test]
    fn a_backlog_doesnt_go_out_in_one_burst() {
        let (a, b) = ([1; 6], [2; 6]);
        let t = Instant::now();
        let mut m = Mixer::default();
        assert!(m.push(b, &pcm(0, 1), t).is_empty());
        for i in 0..5 {
            assert!(m.push(a, &pcm(i, MIC_SAMPLES), t).is_empty());
        }
        let out = m.push(a, &pcm(5, MIC_SAMPLES), t + MIX_GRACE);
        assert_eq!(out.len(), MIX_BURST);
        assert_eq!(out[MIX_BURST - 1], vec![5; MIC_SAMPLES], "the newest kept");
    }

    #[test]
    fn agc_raises_a_quiet_mic_and_leaves_silence() {
        let mut agc = Agc::default();
        let mut quiet = vec![1000i16; MIC_SAMPLES];
        for _ in 0..500 {
            quiet = vec![1000; MIC_SAMPLES];
            agc.apply(&mut quiet);
        }
        assert!((AGC_TARGET * 0.9..=AGC_TARGET * 1.1).contains(&f32::from(quiet[0])), "{}", quiet[0]);
        let mut hiss = vec![10i16; MIC_SAMPLES];
        let gain = agc.gain;
        agc.apply(&mut hiss);
        assert_eq!(agc.gain, gain, "below the gate, the gain holds");
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
        let mut p = Jitter::new(START_FRAMES, MAX_FRAMES);
        p.frames.extend(std::iter::repeat_n([0i16; 2], START_FRAMES - 1));
        assert!(p.take(UNIT_FRAMES).is_none());
        p.frames.push_back([0; 2]);
        assert_eq!(p.take(UNIT_FRAMES).map(|u| u.len()), Some(UNIT_FRAMES));
        p.frames.truncate(UNIT_FRAMES - 1);
        assert!(p.take(UNIT_FRAMES).is_none());
        assert!(!p.playing);
        p.frames.extend(std::iter::repeat_n([0i16; 2], MAX_FRAMES * 2));
        p.trim();
        assert_eq!(p.frames.len(), START_FRAMES);
    }

    #[test]
    fn a_big_ask_waits_for_that_much() {
        let mut p = Jitter::new(10, 1000);
        p.frames.extend(0..50);
        assert!(p.take(60).is_none(), "more than there is");
        assert!(!p.playing, "and not stuck playing with too little");
        p.frames.extend(50..60);
        assert_eq!(p.take(60).map(|d| d.count()), Some(60));
    }
}
