//! Microphone capture and speaker playback for voice calls.
//!
//! Everything here speaks one format, 48 kHz interleaved stereo `f32`, which is
//! what Discord's Opus streams use. The device may run at another rate or
//! channel count; `capture_frames` and `Playback` convert at the edge.
//!
//! cpal streams must stay on the thread that built them, so one dedicated
//! thread owns both streams and exits when `Audio` is dropped. The rest of the
//! app talks to it through lock-free ring buffers:
//!
//! - capture: device callback -> ring -> `MicSource` (read by the Opus encoder)
//! - playback: `Audio::play` (decoded voices) -> ring -> device callback

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
};
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapCons, HeapProd, HeapRb,
};
use songbird::input::core::io::MediaSource;

pub const RATE: u32 = 48_000;
/// One second of stereo audio.
const RING_SAMPLES: usize = RATE as usize * 2;

/// A running audio session. Dropping it stops both streams.
pub struct Audio {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    playback: Mutex<HeapProd<f32>>,
    mic: Mutex<Option<MicSource>>,
    /// Loudness of the microphone, 0.0 to 1.0, as `f32` bits.
    level: Arc<AtomicU32>,
    closed: Arc<AtomicBool>,
}

impl Audio {
    /// Opens the default input and output devices. A missing microphone is
    /// not fatal (the call is listen-only); a missing speaker is.
    pub fn start() -> Result<Self, String> {
        let prefs = crate::prefs::get();
        set_gain(true, prefs.input_volume);
        set_gain(false, prefs.output_volume);
        let (capture_prod, capture_cons) = HeapRb::<f32>::new(RING_SAMPLES).split();
        let (play_prod, play_cons) = HeapRb::<f32>::new(RING_SAMPLES).split();
        let level = Arc::new(AtomicU32::new(0));
        let closed = Arc::new(AtomicBool::new(false));

        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<bool, String>>();
        let thread_level = level.clone();
        let thread = thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(capture_prod, play_cons, thread_level, ready_tx, stop_rx))
            .map_err(|e| e.to_string())?;

        let has_mic = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            stop: stop_tx,
            thread: Some(thread),
            playback: Mutex::new(play_prod),
            mic: Mutex::new(Some(MicSource { ring: Mutex::new(capture_cons), closed: closed.clone(), live: has_mic })),
            level,
            closed,
        })
    }

    /// Queues decoded stereo samples for the speakers.
    pub fn play(&self, samples: &[f32]) {
        if let Ok(mut ring) = self.playback.lock() {
            ring.push_slice(samples);
        }
    }

    /// The microphone as an audio source. Only the first call returns it.
    pub fn take_mic(&self) -> Option<MicSource> {
        self.mic.lock().ok()?.take()
    }

    pub fn mic_level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The microphone as a raw f32 stream for songbird's Opus encoder.
///
/// IMPORTANT: `read` blocks until audio arrives instead of returning silence
/// or zero bytes. Zero bytes would end the track; instant silence would be
/// produced faster than real time and queue up seconds of delay ahead of
/// the real microphone.
pub struct MicSource {
    ring: Mutex<HeapCons<f32>>,
    closed: Arc<AtomicBool>,
    /// False without a microphone: the source then emits paced silence.
    live: bool,
}

impl Read for MicSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let wanted = (out.len() / 8) * 2; // whole stereo frames of 4-byte samples
        if wanted == 0 {
            return Ok(0);
        }
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return Ok(0);
            }
            if !self.live {
                thread::sleep(Duration::from_millis(20));
                let frames = (RATE as usize / 50).min(wanted / 2);
                out[..frames * 8].fill(0);
                return Ok(frames * 8);
            }
            let mut ring = self.ring.lock().map_err(|_| io::Error::other("microphone ring poisoned"))?;
            let available = ring.occupied_len() & !1;
            if available >= 2 {
                let take = available.min(wanted);
                let mut samples = vec![0.0f32; take];
                ring.pop_slice(&mut samples);
                for (chunk, sample) in out.chunks_exact_mut(4).zip(&samples) {
                    chunk.copy_from_slice(&sample.to_le_bytes());
                }
                return Ok(take * 4);
            }
            drop(ring);
            thread::sleep(Duration::from_millis(4));
        }
    }
}

impl Seek for MicSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for MicSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// PipeWire when running, so devices carry the names the desktop shows
/// ("Built-in Audio Analog Stereo") instead of raw ALSA entries.
fn host() -> cpal::Host {
    cpal::host_from_id(cpal::HostId::PipeWire).unwrap_or_else(|_| cpal::default_host())
}

/// Voice volumes as `f32` bits, readable from the audio callbacks without locks.
static INPUT_GAIN: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0
static OUTPUT_GAIN: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// Sets the microphone (`input`) or speaker volume; 1.0 leaves it unchanged.
pub fn set_gain(input: bool, gain: f32) {
    let cell = if input { &INPUT_GAIN } else { &OUTPUT_GAIN };
    cell.store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
}

pub fn gain(input: bool) -> f32 {
    f32::from_bits(if input { &INPUT_GAIN } else { &OUTPUT_GAIN }.load(Ordering::Relaxed))
}

/// A real input or output device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceChoice {
    /// Stable id, saved in the preferences.
    pub id: String,
    /// What the desktop calls it, e.g. "Built-in Audio Analog Stereo".
    pub label: String,
}

/// Whether a PipeWire node id is real hardware of the wanted direction. The
/// backend also lists the "default" aliases, other apps' streams, and each
/// speaker's monitor as an input; none of those are what a person picks.
fn is_hardware(id: &str, input: bool) -> bool {
    let marker = if input { "_input." } else { "_output." };
    id.contains(marker) && !id.ends_with("_default")
}

/// The devices worth offering, in the order the system lists them.
pub fn devices(input: bool) -> Vec<DeviceChoice> {
    let host = host();
    let found = if input { host.input_devices() } else { host.output_devices() };
    found
        .map(|devices| {
            devices
                .filter_map(|d| {
                    let id = d.id().ok()?.to_string();
                    let label = d.description().ok()?.name().to_owned();
                    is_hardware(&id, input).then_some(DeviceChoice { id, label })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The device with this id, or the system default when unset or unplugged.
fn pick(host: &cpal::Host, id: Option<&str>, output: bool) -> Option<cpal::Device> {
    let all = if output { host.output_devices() } else { host.input_devices() };
    if let (Some(id), Ok(mut all)) = (id, all) {
        if let Some(found) = all.find(|d| d.id().is_ok_and(|i| i.to_string() == id)) {
            return Some(found);
        }
    }
    if output { host.default_output_device() } else { host.default_input_device() }
}

/// Body of the audio thread: builds the streams, reports, then waits for stop.
fn run(
    capture: HeapProd<f32>,
    playback: HeapCons<f32>,
    level: Arc<AtomicU32>,
    ready: mpsc::Sender<Result<bool, String>>,
    stop: mpsc::Receiver<()>,
) {
    let host = host();
    let prefs = crate::prefs::get();
    let Some(output) = pick(&host, prefs.output_device.as_deref(), true) else {
        let _ = ready.send(Err("No audio output device found".into()));
        return;
    };
    let speaker = match build_output(&output, playback) {
        Ok(stream) => stream,
        Err(e) => {
            let _ = ready.send(Err(format!("Cannot open the speakers: {e}")));
            return;
        }
    };
    // The microphone is optional; remember why it is missing for the log.
    let mic = pick(&host, prefs.input_device.as_deref(), false)
        .ok_or_else(|| "no input device".to_owned())
        .and_then(|device| build_input(&device, capture, level));
    if let Err(e) = &mic {
        eprintln!("audio: no microphone ({e}); this call is listen-only");
    }
    let started = speaker.play().map_err(|e| e.to_string()).and_then(|()| match &mic {
        Ok(stream) => stream.play().map_err(|e| e.to_string()),
        Err(_) => Ok(()),
    });
    if let Err(e) = started {
        let _ = ready.send(Err(format!("Cannot start audio: {e}")));
        return;
    }
    let _ = ready.send(Ok(mic.is_ok()));
    let _ = stop.recv(); // blocks until `Audio` is dropped; streams stop when they drop
    drop((speaker, mic));
}

fn build_input(
    device: &cpal::Device,
    mut ring: HeapProd<f32>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, String> {
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let mut converter = Converter::new(rate, RATE);

    fn build<T: SizedSample + Send + 'static>(
        device: &cpal::Device,
        config: StreamConfig,
        mut on_samples: impl FnMut(&[T]) + Send + 'static,
    ) -> Result<cpal::Stream, String>
    where
        f32: FromSample<T>,
    {
        device
            .build_input_stream(config, move |data: &[T], _| on_samples(data), |e| eprintln!("audio input: {e}"), None)
            .map_err(|e| e.to_string())
    }

    let mut staged: Vec<f32> = Vec::with_capacity(4096);
    macro_rules! stream {
        ($t:ty) => {
            build::<$t>(device, config.clone(), move |data| {
                staged.clear();
                let mut peak = 0.0f32;
                let volume = gain(true);
                for frame in data.chunks_exact(channels) {
                    let left: f32 = frame[0].to_sample();
                    let right: f32 = if channels > 1 { frame[1].to_sample() } else { left };
                    peak = peak.max(left.abs()).max(right.abs());
                    converter.push([(left * volume).clamp(-1.0, 1.0), (right * volume).clamp(-1.0, 1.0)], &mut staged);
                }
                ring.push_slice(&staged);
                // Fast attack, slow release, so a short syllable still registers.
                let old = f32::from_bits(level.load(Ordering::Relaxed));
                level.store(peak.max(old * 0.9).to_bits(), Ordering::Relaxed);
            })
        };
    }
    match supported.sample_format() {
        SampleFormat::F32 => stream!(f32),
        SampleFormat::I16 => stream!(i16),
        SampleFormat::U16 => stream!(u16),
        SampleFormat::I32 => stream!(i32),
        other => Err(format!("unsupported microphone format {other:?}")),
    }
}

fn build_output(device: &cpal::Device, mut ring: HeapCons<f32>) -> Result<cpal::Stream, String> {
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let channels = usize::from(config.channels);
    let mut resampler = Resampler::new(RATE, config.sample_rate);

    fn build<T: SizedSample + Send + 'static>(
        device: &cpal::Device,
        config: StreamConfig,
        mut fill: impl FnMut(&mut [T]) + Send + 'static,
    ) -> Result<cpal::Stream, String>
    where
        T: FromSample<f32>,
    {
        device
            .build_output_stream(config, move |data: &mut [T], _| fill(data), |e| eprintln!("audio output: {e}"), None)
            .map_err(|e| e.to_string())
    }

    macro_rules! stream {
        ($t:ty) => {
            build::<$t>(device, config.clone(), move |data| {
                for frame in data.chunks_exact_mut(channels) {
                    let [left, right] = resampler.next(|| {
                        let mut pair = [0.0f32; 2];
                        if ring.occupied_len() >= 2 {
                            ring.pop_slice(&mut pair);
                        }
                        pair
                    });
                    for (i, out) in frame.iter_mut().enumerate() {
                        let value = match (channels, i) {
                            (1, _) => (left + right) * 0.5,
                            (_, 0) => left,
                            (_, 1) => right,
                            _ => 0.0,
                        };
                        *out = value.to_sample();
                    }
                }
            })
        };
    }
    match supported.sample_format() {
        SampleFormat::F32 => stream!(f32),
        SampleFormat::I16 => stream!(i16),
        SampleFormat::U16 => stream!(u16),
        SampleFormat::I32 => stream!(i32),
        other => Err(format!("unsupported speaker format {other:?}")),
    }
}

/// Linear-interpolation rate conversion for pushed input frames.
struct Converter {
    step: f64,
    phase: f64,
    previous: [f32; 2],
}

impl Converter {
    fn new(from: u32, to: u32) -> Self {
        Self { step: f64::from(from) / f64::from(to), phase: 0.0, previous: [0.0; 2] }
    }

    fn push(&mut self, frame: [f32; 2], out: &mut Vec<f32>) {
        while self.phase < 1.0 {
            let t = self.phase as f32;
            out.push(self.previous[0] * (1.0 - t) + frame[0] * t);
            out.push(self.previous[1] * (1.0 - t) + frame[1] * t);
            self.phase += self.step;
        }
        self.phase -= 1.0;
        self.previous = frame;
    }
}

/// Linear-interpolation rate conversion for pulled output frames.
struct Resampler {
    step: f64,
    phase: f64,
    previous: [f32; 2],
    current: [f32; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: f64::from(from) / f64::from(to), phase: 1.0, previous: [0.0; 2], current: [0.0; 2] }
    }

    fn next(&mut self, mut pull: impl FnMut() -> [f32; 2]) -> [f32; 2] {
        while self.phase >= 1.0 {
            self.previous = self.current;
            self.current = pull();
            self.phase -= 1.0;
        }
        let t = self.phase as f32;
        self.phase += self.step;
        [
            self.previous[0] * (1.0 - t) + self.current[0] * t,
            self.previous[1] * (1.0 - t) + self.current[1] * t,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_preserves_duration() {
        // 44.1 kHz in, 48 kHz out: one second in gives about one second out.
        let mut c = Converter::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..44_100 {
            c.push([0.5, -0.5], &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
    }

    #[test]
    fn resampler_pulls_the_right_amount() {
        // 48 kHz ring into a 44.1 kHz device: 44_100 outputs consume about 48_000 inputs.
        let mut r = Resampler::new(48_000, 44_100);
        let mut pulled = 0;
        for _ in 0..44_100 {
            r.next(|| {
                pulled += 1;
                [0.25; 2]
            });
        }
        assert!((pulled as i64 - 48_000).abs() <= 3, "{pulled}");
    }

    #[test]
    fn mic_source_blocks_until_data_and_ends_when_closed() {
        let (mut prod, cons) = HeapRb::<f32>::new(64).split();
        let closed = Arc::new(AtomicBool::new(false));
        let mut mic = MicSource { ring: Mutex::new(cons), closed: closed.clone(), live: true };
        prod.push_slice(&[0.5, -0.5, 0.25, -0.25]);
        let mut buf = [0u8; 64];
        let n = mic.read(&mut buf).unwrap();
        assert_eq!(n, 16);
        assert_eq!(f32::from_le_bytes(buf[0..4].try_into().unwrap()), 0.5);
        closed.store(true, Ordering::Relaxed);
        assert_eq!(mic.read(&mut buf).unwrap(), 0);
    }
}


#[cfg(test)]
mod hardware_filter {
    use super::is_hardware;

    #[test]
    fn keeps_real_devices_only() {
        assert!(is_hardware("pipewire:alsa_input.usb-fifine.analog-stereo", true));
        assert!(!is_hardware("pipewire:alsa_output.usb-fifine.analog-stereo", true));
        assert!(is_hardware("pipewire:alsa_output.pci-0000_00_1f.3.analog-stereo", false));
        assert!(!is_hardware("pipewire:input_default", true));
        assert!(!is_hardware("pipewire:spotify", false));
    }
}
