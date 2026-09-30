//! Audio output. The real-time callback reads control changes from atomics and
//! ramps every level change so nothing clicks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::noise::{Color, StereoLoop};

const START_STOP_FADE_SECS: f32 = 1.5;
const CROSSFADE_SECS: f32 = 1.0;
const VOLUME_SMOOTHING_SECS: f32 = 0.05;
/// Range of the volume slider: 1 → -50 dB, 100 → 0 dB.
const VOLUME_RANGE_DB: f32 = 50.0;

/// Desired player state, written by the control side, read by the audio thread.
pub struct Controls {
    playing: AtomicBool,
    color: AtomicU8,
    volume: AtomicU8,
    /// Linear gain derived from `volume`, stored as f32 bits.
    gain: AtomicU32,
}

impl Controls {
    pub fn new(color: Color, volume: u8) -> Arc<Controls> {
        let c = Controls {
            playing: AtomicBool::new(false),
            color: AtomicU8::new(color.index() as u8),
            volume: AtomicU8::new(0),
            gain: AtomicU32::new(0),
        };
        c.set_volume(volume);
        Arc::new(c)
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn set_playing(&self, on: bool) {
        self.playing.store(on, Ordering::Relaxed);
    }

    pub fn color(&self) -> Color {
        Color::from_index(self.color.load(Ordering::Relaxed) as usize)
    }

    pub fn set_color(&self, color: Color) {
        self.color.store(color.index() as u8, Ordering::Relaxed);
    }

    pub fn volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed)
    }

    /// Volume 0–100 on a perceptual (dB) curve.
    pub fn set_volume(&self, volume: u8) {
        let volume = volume.min(100);
        let gain = if volume == 0 {
            0.0
        } else {
            10f32.powf((volume as f32 - 100.0) / 100.0 * VOLUME_RANGE_DB / 20.0)
        };
        self.volume.store(volume, Ordering::Relaxed);
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }
}

pub fn find_device(name: Option<&str>) -> anyhow::Result<Device> {
    let host = cpal::default_host();
    match name {
        None | Some("default") => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device")),
        Some(name) => {
            if let Ok(id) = name.parse()
                && let Some(dev) = host.device_by_id(&id)
            {
                return Ok(dev);
            }
            host.output_devices()?
                .find(|d| {
                    d.description().is_ok_and(|desc| desc.name().contains(name))
                        || d.id().is_ok_and(|id| id.to_string().contains(name))
                })
                .ok_or_else(|| anyhow!("no output device matching {name:?}"))
        }
    }
}

pub fn list_devices() -> anyhow::Result<()> {
    let host = cpal::default_host();
    for dev in host.output_devices()? {
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
        let id = dev.id().map(|id| id.to_string()).unwrap_or_default();
        let cfg = dev
            .default_output_config()
            .map(|c| format!("{} ch, {} Hz, {:?}", c.channels(), c.sample_rate(), c.sample_format()))
            .unwrap_or_else(|e| format!("({e})"));
        println!("{name}\n    id: {id}\n    default: {cfg}");
    }
    Ok(())
}

/// The device's default config: the sample rate the loops must be generated at.
pub fn output_config(device: &Device) -> anyhow::Result<(StreamConfig, SampleFormat)> {
    let cfg = device
        .default_output_config()
        .context("querying output config")?;
    Ok((cfg.config(), cfg.sample_format()))
}

/// Start the output stream. Playback continues until the returned `Stream` is dropped.
/// `loops` is indexed by `Color::index()` and every loop must be the same length.
pub fn start(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    loops: Arc<Vec<StereoLoop>>,
    controls: Arc<Controls>,
) -> anyhow::Result<Stream> {
    let stream = match format {
        SampleFormat::F32 => build::<f32>(device, config, loops, controls),
        SampleFormat::I16 => build::<i16>(device, config, loops, controls),
        SampleFormat::I24 => build::<cpal::I24>(device, config, loops, controls),
        SampleFormat::I32 => build::<i32>(device, config, loops, controls),
        SampleFormat::U16 => build::<u16>(device, config, loops, controls),
        SampleFormat::F64 => build::<f64>(device, config, loops, controls),
        other => Err(anyhow!("unsupported sample format {other}")),
    }?;
    stream.play()?;
    Ok(stream)
}

fn build<T>(
    device: &Device,
    config: StreamConfig,
    loops: Arc<Vec<StereoLoop>>,
    controls: Arc<Controls>,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut mixer = Mixer::new(config.sample_rate as f32, loops, controls);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            for frame in data.chunks_mut(channels) {
                let (l, r) = mixer.next_frame();
                match frame {
                    [mono] => *mono = T::from_sample(0.5 * (l + r)),
                    [a, b, rest @ ..] => {
                        *a = T::from_sample(l);
                        *b = T::from_sample(r);
                        rest.fill(T::from_sample(0.0));
                    }
                    [] => {}
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// Per-sample state owned by the audio thread.
struct Mixer {
    loops: Arc<Vec<StereoLoop>>,
    controls: Arc<Controls>,
    pos: usize,
    /// 0..1 start/stop envelope.
    envelope: f32,
    envelope_step: f32,
    gain: f32,
    gain_coef: f32,
    current: usize,
    /// Color being faded out, with crossfade progress 0..1.
    fading_from: Option<(usize, f32)>,
    crossfade_step: f32,
}

impl Mixer {
    fn new(sample_rate: f32, loops: Arc<Vec<StereoLoop>>, controls: Arc<Controls>) -> Mixer {
        Mixer {
            current: controls.color().index(),
            gain: controls.gain(),
            loops,
            controls,
            pos: 0,
            envelope: 0.0,
            envelope_step: 1.0 / (START_STOP_FADE_SECS * sample_rate),
            gain_coef: 1.0 - (-1.0 / (VOLUME_SMOOTHING_SECS * sample_rate)).exp(),
            fading_from: None,
            crossfade_step: 1.0 / (CROSSFADE_SECS * sample_rate),
        }
    }

    fn next_frame(&mut self) -> (f32, f32) {
        let playing = self.controls.playing();
        let target_color = self.controls.color().index();
        self.gain += (self.controls.gain() - self.gain) * self.gain_coef;

        if self.envelope == 0.0 && !playing {
            // Silent: switch color instantly and don't bother reading the loop.
            self.current = target_color;
            self.fading_from = None;
            return (0.0, 0.0);
        }
        self.envelope = if playing {
            (self.envelope + self.envelope_step).min(1.0)
        } else {
            (self.envelope - self.envelope_step).max(0.0)
        };

        if target_color != self.current && self.fading_from.is_none() {
            self.fading_from = Some((self.current, 0.0));
            self.current = target_color;
        }

        let cur = &self.loops[self.current];
        let (mut l, mut r) = (cur.left[self.pos], cur.right[self.pos]);
        if let Some((from, t)) = self.fading_from {
            // Equal-power crossfade: the loops are uncorrelated.
            let angle = t * std::f32::consts::FRAC_PI_2;
            let (fade_in, fade_out) = (angle.sin(), angle.cos());
            let old = &self.loops[from];
            l = l * fade_in + old.left[self.pos] * fade_out;
            r = r * fade_in + old.right[self.pos] * fade_out;
            let t = t + self.crossfade_step;
            self.fading_from = (t < 1.0).then_some((from, t));
        }

        self.pos += 1;
        if self.pos == cur.len() {
            self.pos = 0;
        }
        // Squared envelope sounds smoother than linear at the quiet end.
        let g = self.gain * self.envelope * self.envelope;
        (l * g, r * g)
    }
}
