//! Seamless noise loops via spectral synthesis.
//!
//! We build the spectrum directly (magnitude by noise color, random phase) and
//! inverse-FFT it. The result is exactly periodic, so the loop has no seam.

use std::f32::consts::TAU;

use rand::{Rng, RngExt};
use realfft::{RealFftPlanner, num_complex::Complex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Color {
    White,
    Pink,
    Brown,
}

impl Color {
    pub const ALL: [Color; 3] = [Color::White, Color::Pink, Color::Brown];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(i: usize) -> Color {
        Self::ALL[i]
    }

    pub fn name(self) -> &'static str {
        match self {
            Color::White => "white",
            Color::Pink => "pink",
            Color::Brown => "brown",
        }
    }

    /// Spectral magnitude slope: |X(f)| ∝ f^exponent.
    fn exponent(self) -> f32 {
        match self {
            Color::White => 0.0,
            Color::Pink => -0.5,
            Color::Brown => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LoopParams {
    pub sample_rate: u32,
    pub seconds: f32,
    /// Corner of a 2nd-order Butterworth-shaped high-pass; 0 disables it.
    pub highpass_hz: f32,
    /// Peak level of the loop in dBFS.
    pub peak_dbfs: f32,
}

impl Default for LoopParams {
    fn default() -> Self {
        LoopParams {
            sample_rate: 48_000,
            seconds: 60.0,
            highpass_hz: 20.0,
            peak_dbfs: -3.0,
        }
    }
}

/// A stereo loop with the same number of frames in each channel.
pub struct StereoLoop {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl StereoLoop {
    pub fn len(&self) -> usize {
        self.left.len()
    }
}

/// Generate a stereo loop with independent (decorrelated) channels.
pub fn generate(color: Color, params: &LoopParams, rng: &mut impl Rng) -> StereoLoop {
    // Even length keeps the Nyquist bin real.
    let n = ((params.seconds * params.sample_rate as f32) as usize).max(2) & !1;
    let mut planner = RealFftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(n);

    let magnitudes = spectrum_magnitudes(color, params, n);
    let mut channel = || {
        let mut spectrum: Vec<Complex<f32>> = magnitudes
            .iter()
            .map(|&m| Complex::from_polar(m, rng.random::<f32>() * TAU))
            .collect();
        // DC and Nyquist bins must be purely real.
        spectrum[0] = Complex::new(0.0, 0.0);
        let last = spectrum.len() - 1;
        spectrum[last] = Complex::new(spectrum[last].norm(), 0.0);

        let mut out = ifft.make_output_vec();
        ifft.process(&mut spectrum, &mut out)
            .expect("buffer sizes come from the planner");
        out
    };
    let mut left = channel();
    let mut right = channel();

    // One gain for both channels keeps the stereo image balanced.
    let peak = left
        .iter()
        .chain(right.iter())
        .fold(0.0f32, |p, s| p.max(s.abs()));
    if peak > 0.0 {
        let gain = 10f32.powf(params.peak_dbfs / 20.0) / peak;
        left.iter_mut().chain(right.iter_mut()).for_each(|s| *s *= gain);
    }
    StereoLoop { left, right }
}

fn spectrum_magnitudes(color: Color, params: &LoopParams, n: usize) -> Vec<f32> {
    let bin_hz = params.sample_rate as f32 / n as f32;
    (0..=n / 2)
        .map(|k| {
            if k == 0 {
                return 0.0;
            }
            let f = k as f32 * bin_hz;
            let mut m = f.powf(color.exponent());
            if params.highpass_hz > 0.0 {
                let r2 = (f / params.highpass_hz).powi(2);
                m *= r2 / (1.0 + r2 * r2).sqrt();
            }
            m
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn params() -> LoopParams {
        LoopParams {
            sample_rate: 48_000,
            seconds: 5.0,
            ..Default::default()
        }
    }

    /// Mean power in a frequency band, from the forward FFT of the loop.
    fn band_power(x: &[f32], sr: f32, lo: f32, hi: f32) -> f32 {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(x.len());
        let mut input = x.to_vec();
        let mut spec = fft.make_output_vec();
        fft.process(&mut input, &mut spec).unwrap();
        let bin_hz = sr / x.len() as f32;
        let (a, b) = ((lo / bin_hz) as usize, (hi / bin_hz) as usize);
        spec[a..b].iter().map(|c| c.norm_sqr()).sum::<f32>() / (b - a) as f32
    }

    #[test]
    fn spectral_slopes_per_octave() {
        let p = params();
        for (color, expected_db) in [(Color::White, 0.0), (Color::Pink, -3.0), (Color::Brown, -6.0)] {
            let lp = generate(color, &p, &mut ChaCha8Rng::seed_from_u64(1));
            // Compare two octave-wide bands well above the high-pass corner.
            let p1 = band_power(&lp.left, 48_000.0, 500.0, 1000.0);
            let p2 = band_power(&lp.left, 48_000.0, 1000.0, 2000.0);
            let db = 10.0 * (p2 / p1).log10();
            assert!((db - expected_db).abs() < 0.5, "{color:?}: {db} dB/octave");
        }
    }

    #[test]
    fn loop_seam_is_not_special() {
        let lp = generate(Color::Brown, &params(), &mut ChaCha8Rng::seed_from_u64(2));
        let x = &lp.left;
        let deltas: Vec<f32> = x.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = deltas.iter().sum::<f32>() / deltas.len() as f32;
        let seam = (x[0] - x[x.len() - 1]).abs();
        // A seam from a non-periodic signal would be orders of magnitude larger.
        assert!(seam < mean * 6.0, "seam {seam} vs mean delta {mean}");
    }

    #[test]
    fn peak_is_normalized() {
        let p = params();
        let lp = generate(Color::Pink, &p, &mut ChaCha8Rng::seed_from_u64(3));
        let peak = lp.left.iter().chain(&lp.right).fold(0.0f32, |a, s| a.max(s.abs()));
        let target = 10f32.powf(p.peak_dbfs / 20.0);
        assert!((peak - target).abs() < 1e-4);
    }
}
