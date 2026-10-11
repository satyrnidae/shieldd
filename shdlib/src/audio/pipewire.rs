//! Controller audio as a mic and a headset of shieldd's own in the desktop's sound settings: a
//! PipeWire source the mix goes out on, and a sink whose audio goes to every headset. Both are
//! plain streams that say they're devices (media.class), as pw-loopback's virtual sinks do, so
//! they're there exactly as long as audio is on, crash or not. PipeWire converts to and from
//! the controller's own formats.

use std::io::Cursor;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{Stream, StreamBox, StreamFlags, StreamState};
use spa::param::ParamType;
use spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Value};
use spa::utils::{Direction, SpaTypes};
use tracing::{debug, info, warn};

use super::{HEADSET_NAME, Headset, Jitter, MIC_NAME, MIC_RATE, MIC_SAMPLES, PAD_RATE, lock};

/// The mic starts once this much of the mix is in, 40 ms, and is trimmed back to it past 200 ms
const MIC_START: usize = MIC_SAMPLES * 4;
const MIC_MAX: usize = MIC_SAMPLES * 20;

/// Whether there's a PipeWire daemon to connect to, from its socket, where libpipewire looks
pub(super) fn running() -> bool {
    if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    let dir = ["PIPEWIRE_RUNTIME_DIR", "XDG_RUNTIME_DIR"].into_iter().find_map(std::env::var_os);
    dir.is_some_and(|d| Path::new(&d).join("pipewire-0").exists())
}

/// Both nodes, on their own thread's main loop
pub(super) struct PipeWire {
    mic: Arc<Mutex<Jitter<i16>>>,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWire {
    /// Headset audio goes onto `pad`
    pub(super) fn start(pad: Arc<Mutex<Jitter<[i16; 2]>>>) -> Result<PipeWire> {
        let mic = Arc::new(Mutex::new(Jitter::new(MIC_START, MIC_MAX)));
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready) = mpsc::channel();
        let thread = {
            let mic = mic.clone();
            std::thread::Builder::new().name("pipewire".into()).spawn(move || {
                if let Err(e) = run(mic, pad, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?
        };
        let mut pw = PipeWire { mic, quit, thread: Some(thread) };
        match ready.recv() {
            Ok(Ok(())) => {
                info!(mic = MIC_NAME, headset = HEADSET_NAME, "PipeWire audio on");
                Ok(pw)
            }
            Ok(Err(e)) => {
                pw.join();
                Err(e)
            }
            Err(_) => Err(anyhow!("the PipeWire thread died")),
        }
    }

    /// One packet of the mix
    pub(super) fn push_mic(&mut self, mono: &[i16]) {
        let mut m = lock(&self.mic);
        m.frames.extend(mono);
        m.trim();
        m.report("mic");
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PipeWire {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        self.join();
        info!("PipeWire audio off");
    }
}

/// The main loop, with both nodes up, until `quit`. Says on `ready` once they are.
fn run(
    mic: Arc<Mutex<Jitter<i16>>>,
    pad: Arc<Mutex<Jitter<[i16; 2]>>>,
    quit: pw::channel::Receiver<()>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let _core = core.add_listener_local().error(|id, _, res, message| warn!(id, res, "PipeWire: {message}")).register();

    let source = StreamBox::new(
        &core,
        "shieldd mic",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => "shieldd.mic",
            *pw::keys::NODE_DESCRIPTION => MIC_NAME,
        },
    )?;
    let _source = source
        .add_local_listener_with_user_data(mic)
        .state_changed(|_, _, _, new| state("mic", new))
        .process(fill_mic)
        .register()?;
    let mic_format = format(MIC_RATE, &[spa::sys::SPA_AUDIO_CHANNEL_MONO]);
    let mut params = [Pod::from_bytes(&mic_format).expect("a pod")];
    source.connect(Direction::Output, None, StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS, &mut params)?;

    let sink = StreamBox::new(
        &core,
        "shieldd headset",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "shieldd.headset",
            *pw::keys::NODE_DESCRIPTION => HEADSET_NAME,
        },
    )?;
    let _sink = sink
        .add_local_listener_with_user_data((pad, Headset::default()))
        .state_changed(|_, _, _, new| state("headset", new))
        .process(drain_headset)
        .register()?;
    let pad_format = format(PAD_RATE, &[spa::sys::SPA_AUDIO_CHANNEL_FL, spa::sys::SPA_AUDIO_CHANNEL_FR]);
    let mut params = [Pod::from_bytes(&pad_format).expect("a pod")];
    sink.connect(Direction::Input, None, StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS, &mut params)?;

    let weak = main.downgrade();
    let _quit = quit.attach(main.loop_(), move |()| {
        if let Some(main) = weak.upgrade() {
            main.quit();
        }
    });
    let _ = ready.send(Ok(()));
    main.run();
    Ok(())
}

fn state(stream: &str, new: StreamState) {
    match new {
        StreamState::Error(e) => warn!(stream, "PipeWire: {e}"),
        s => debug!(stream, state = ?s, "PipeWire"),
    }
}

/// s16le at `rate`, in these channel positions
fn format(rate: u32, channels: &[u32]) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(rate);
    info.set_channels(channels.len() as u32);
    let mut position = [0; MAX_CHANNELS];
    position[..channels.len()].copy_from_slice(channels);
    info.set_position(position);
    let format = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(format)).expect("an audio format").0.into_inner()
}

/// Whoever's recording from the mic wants the next of the mix; silence while there's too little
fn fill_mic(stream: &Stream, mic: &mut Arc<Mutex<Jitter<i16>>>) {
    let Some(mut buffer) = stream.dequeue_buffer() else { return };
    let requested = buffer.requested() as usize;
    let Some(d) = buffer.datas_mut().first_mut() else { return };
    let Some(out) = d.data() else { return };
    let n = match requested {
        0 => out.len() / 2,
        r => r.min(out.len() / 2),
    };
    match lock(mic).take(n) {
        Some(mono) => {
            for (b, x) in out.as_chunks_mut::<2>().0.iter_mut().zip(mono) {
                b.copy_from_slice(&x.to_le_bytes());
            }
        }
        None => out[..n * 2].fill(0),
    }
    let chunk = d.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = 2;
    *chunk.size_mut() = (n * 2) as u32;
}

/// What's played to the headset, onto the jitter buffer
fn drain_headset(stream: &Stream, (pad, headset): &mut (Arc<Mutex<Jitter<[i16; 2]>>>, Headset)) {
    let Some(mut buffer) = stream.dequeue_buffer() else { return };
    let Some(d) = buffer.datas_mut().first_mut() else { return };
    let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
    let Some(pcm) = d.data().and_then(|b| b.get(offset..offset + size)) else { return };
    headset.push(pad, PAD_RATE, 2, pcm);
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::super::{MAX_FRAMES, PAD_GAIN, START_FRAMES};
    use super::*;

    /// pw-cat, raw s16, at one of our nodes
    fn pw_cat(mode: &str, target: &str, rate: u32, channels: u8) -> std::process::Child {
        let (rate, channels) = (rate.to_string(), channels.to_string());
        Command::new("pw-cat")
            .args([mode, "--raw", "--format", "s16", "--rate", &rate, "--channels", &channels, "--target", target, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("pw-cat")
    }

    /// Both nodes up, as apps see them: a tone played to the headset lands in the jitter buffer,
    /// and the mic records what's pushed to it. Needs a running PipeWire and pw-cat:
    /// `cargo test -p shdlib --features pipewire -- --ignored pipewire`
    #[test]
    #[ignore = "needs a running PipeWire"]
    fn pipewire_end_to_end() {
        let pad = Arc::new(Mutex::new(Jitter::new(START_FRAMES, MAX_FRAMES)));
        let mut pw = PipeWire::start(pad.clone()).unwrap();

        // Half a second of a 1 kHz square wave, at the headset's own format
        let tone: Vec<u8> =
            (0..16000).flat_map(|i| if i / 16 % 2 == 0 { 8000i16 } else { -8000 }.to_le_bytes().repeat(2)).collect();
        let mut play = pw_cat("--playback", "shieldd.headset", PAD_RATE, 2);
        play.stdin.take().unwrap().write_all(&tone).unwrap();
        assert!(play.wait().unwrap().success());
        let got: Vec<i16> = lock(&pad).frames.iter().map(|f| f[0]).collect();
        // Nothing's taking it out, so it's been trimmed back to where playing starts
        let loud = got.iter().filter(|x| x.abs() > (7000.0 * PAD_GAIN) as i16).count();
        assert!(got.len() >= START_FRAMES && loud > got.len() / 2, "the tone came through: {loud} of {}", got.len());

        // A second of the mix at the mic's pace, recorded as an app would
        let mut rec = pw_cat("--record", "shieldd.mic", MIC_RATE, 1);
        let start = Instant::now();
        for n in 0..100 {
            pw.push_mic(&[3000; MIC_SAMPLES]);
            std::thread::sleep((start + Duration::from_millis(10 * (n + 1))).saturating_duration_since(Instant::now()));
        }
        rec.kill().unwrap();
        let mut out = Vec::new();
        rec.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        let _ = rec.wait();
        let mic: Vec<i16> = out.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b)).collect();
        let heard = mic.iter().filter(|&&x| (2900..=3100).contains(&x)).count();
        assert!(heard > MIC_RATE as usize / 2, "the mic came through: {heard} of {} samples", mic.len());
        drop(pw);
    }
}
