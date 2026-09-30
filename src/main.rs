mod noise;
mod player;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use cpal::traits::DeviceTrait;
use clap::{Args, Parser, Subcommand};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use noise::{Color, LoopParams, StereoLoop};

#[derive(Parser)]
#[command(version, about = "Seamless brown/pink/white noise player")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write one noise loop to a WAV file.
    Render {
        #[arg(long, value_enum, default_value = "brown")]
        color: Color,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        #[command(flatten)]
        noise: NoiseArgs,
        out: PathBuf,
    },
    /// Play noise on an output device, controlled from stdin.
    Play {
        #[arg(long, value_enum, default_value = "brown")]
        color: Color,
        /// Volume 0–100.
        #[arg(long, default_value_t = 40)]
        volume: u8,
        /// Output device name or id (see `devices`).
        #[arg(long)]
        device: Option<String>,
        #[command(flatten)]
        noise: NoiseArgs,
    },
    /// List audio output devices.
    Devices,
}

#[derive(Args)]
struct NoiseArgs {
    /// Loop length in seconds.
    #[arg(long, default_value_t = 60.0)]
    seconds: f32,
    /// High-pass corner in Hz (0 disables).
    #[arg(long, default_value_t = 20.0)]
    highpass: f32,
    /// RNG seed for reproducible output.
    #[arg(long)]
    seed: Option<u64>,
}

impl NoiseArgs {
    fn params(&self, sample_rate: u32) -> LoopParams {
        LoopParams {
            sample_rate,
            seconds: self.seconds,
            highpass_hz: self.highpass,
            ..Default::default()
        }
    }

    fn rng(&self) -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(self.seed.unwrap_or_else(rand::random))
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Render { color, sample_rate, noise, out } => {
            let started = Instant::now();
            let lp = noise::generate(color, &noise.params(sample_rate), &mut noise.rng());
            write_wav(&out, &lp, sample_rate)?;
            println!(
                "wrote {} ({:.1} s of {} noise) in {:.2?}",
                out.display(),
                lp.len() as f32 / sample_rate as f32,
                color.name(),
                started.elapsed()
            );
            Ok(())
        }
        Command::Play { color, volume, device, noise } => play(color, volume, device, noise),
        Command::Devices => player::list_devices(),
    }
}

fn write_wav(path: &PathBuf, lp: &StereoLoop, sample_rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).context("creating WAV file")?;
    for (l, r) in lp.left.iter().zip(&lp.right) {
        w.write_sample(*l)?;
        w.write_sample(*r)?;
    }
    w.finalize()?;
    Ok(())
}

fn play(color: Color, volume: u8, device: Option<String>, noise: NoiseArgs) -> anyhow::Result<()> {
    let device = player::find_device(device.as_deref())?;
    let (config, format) = player::output_config(&device)?;
    println!(
        "device: {}  ({} ch, {} Hz, {format})",
        device.description().map(|d| d.name().to_string()).unwrap_or_default(),
        config.channels,
        config.sample_rate
    );

    let started = Instant::now();
    let params = noise.params(config.sample_rate);
    let mut rng = noise.rng();
    let loops: Vec<StereoLoop> = Color::ALL.iter().map(|&c| noise::generate(c, &params, &mut rng)).collect();
    println!("generated loops in {:.2?}", started.elapsed());

    let controls = player::Controls::new(color, volume);
    let _stream = player::start(&device, config, format, Arc::new(loops), controls.clone())?;
    controls.set_playing(true);

    println!("commands: w/p/b = white/pink/brown, 0-100 = volume, s = start/stop, q = quit");
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        match line.trim() {
            "w" => controls.set_color(Color::White),
            "p" => controls.set_color(Color::Pink),
            "b" => controls.set_color(Color::Brown),
            "s" => controls.set_playing(!controls.playing()),
            "q" => break,
            v => match v.parse::<u8>() {
                Ok(v) => controls.set_volume(v),
                Err(_) => {
                    println!("?");
                    continue;
                }
            },
        }
        println!(
            "{} {} vol {}",
            if controls.playing() { "playing" } else { "stopped" },
            controls.color().name(),
            controls.volume()
        );
    }

    // Let the fade-out finish before the stream is dropped.
    if controls.playing() {
        controls.set_playing(false);
        std::thread::sleep(std::time::Duration::from_millis(1600));
    }
    Ok(())
}
