//! Integer resampling between the codec's rate and the 8 kHz socket rate
//! (D-A5).
//!
//! The driver may only run at 16 kHz (the vendor `ipc` always asked for
//! it), while G.711 and the socket are 8 kHz. Rates are integer multiples of
//! 8 kHz (`board` accepts 8000..=48000 in steps of 8000), so both directions
//! are a polyphase FIR with an integer factor `L = rate / 8000`:
//!
//! ```text
//! capture   rate ──► lowpass ──► keep 1 of L ──► 8 kHz      (Decimator)
//! playback  8 kHz ──► L-1 zeros after each ──► lowpass ×L ──► rate (Interpolator)
//! ```
//!
//! One lowpass for both: a Kaiser-windowed sinc (β 4, cutoff 3.6 kHz,
//! `36·L + 1` taps, 73 at 16 kHz). Measured on the quantised taps: −0.6 dB
//! at 3.4 kHz (the telephone band edge), ≥ 44 dB down from 3.9 kHz, so what
//! would alias into the 8 kHz band is gone.
//!
//! SPEC_DEVIATION (design D-A5 "half-band, 11 taps"): a half-band filter is
//! −6 dB at exactly fs/4 = 4 kHz by construction, so it cannot attenuate
//! 3.9 kHz by 30 dB as required here; the longer windowed sinc does, and
//! still costs only ~37 multiply-accumulates per 8 kHz sample per direction
//! (≈ 0.6 M MAC/s at 16 kHz, a fraction of a percent of the ARM926).
//!
//! Arithmetic is integer only (the SoC has no FPU): taps in Q15, 64-bit
//! accumulators (`smlal` on ARMv5TE), rounded and saturated to i16. The taps
//! are computed once at start in f64 (soft-float, microseconds). Both
//! directions keep their delay line across calls, so periods and frames of
//! any size can be fed without clicks at the seams.

use snafu::{ensure, Snafu};

/// The socket rate (`audio_proto::RATE`).
pub const OUT_RATE: u32 = audio_proto::RATE;

/// Largest supported `rate / 8000` (48 kHz).
pub const MAX_FACTOR: u32 = 6;

/// Passband edge kept intact, Hz (cutoff of the windowed sinc).
const CUTOFF_HZ: f64 = 3600.0;
/// Kaiser window shape: ~45 dB stopband.
const KAISER_BETA: f64 = 4.0;
/// Taps per unit of factor; `TAPS_PER_FACTOR * L + 1` taps in all.
const TAPS_PER_FACTOR: usize = 36;
/// Q15 one.
const ONE: i64 = 1 << 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Snafu)]
pub enum Error {
    #[snafu(display(
        "audio rate {rate} Hz is not a multiple of {OUT_RATE} Hz up to {} Hz",
        OUT_RATE * MAX_FACTOR
    ))]
    UnsupportedRate { rate: u32 },
}

/// `rate / 8000`, if `rate` is one this module can convert.
pub fn factor(rate: u32) -> Result<usize, Error> {
    ensure!(
        rate != 0 && rate.is_multiple_of(OUT_RATE) && rate / OUT_RATE <= MAX_FACTOR,
        UnsupportedRateSnafu { rate }
    );
    Ok((rate / OUT_RATE) as usize)
}

/// The lowpass at `factor * 8000` Hz, unity DC gain, as f64.
fn design(factor: usize) -> Vec<f64> {
    let taps = TAPS_PER_FACTOR * factor + 1;
    let fs = (OUT_RATE as usize * factor) as f64;
    let fc = 2.0 * CUTOFF_HZ / fs; // normalised to Nyquist
    let mid = (taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let n = i as f64 - mid;
            let x = std::f64::consts::PI * fc * n;
            let sinc = if n == 0.0 { 1.0 } else { x.sin() / x };
            let r = n / mid;
            let window = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            fc * sinc * window
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|t| *t /= sum);
    h
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= half / k as f64;
        let t2 = term * term;
        sum += t2;
        if t2 < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Quantises `h` (scaled by `gain`) to Q15 so the taps sum to exactly
/// `gain` in Q15: the rounding error goes into the largest tap.
fn quantise(h: &[f64], gain: f64) -> Vec<i32> {
    let mut q: Vec<i32> = h
        .iter()
        .map(|t| (t * gain * ONE as f64).round() as i32)
        .collect();
    let want = (gain * ONE as f64).round() as i64;
    let have: i64 = q.iter().map(|&t| i64::from(t)).sum();
    if let Some(peak) = (0..q.len()).max_by_key(|&i| q[i].abs()) {
        q[peak] += (want - have) as i32;
    }
    q
}

/// Q15 accumulator to a saturated, rounded sample.
fn to_sample(acc: i64) -> i16 {
    ((acc + (ONE >> 1)) >> 15).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

/// `rate` → 8 kHz. State carries across [`Decimator::process`] calls.
#[derive(Debug, Clone)]
pub struct Decimator {
    factor: usize,
    taps: Vec<i32>,
    /// Last `taps.len() - 1` inputs, then the current call's input.
    line: Vec<i16>,
    /// Position of the next input in its group of `factor`.
    phase: usize,
}

impl Decimator {
    /// For a codec running at `rate` Hz.
    pub fn new(rate: u32) -> Result<Self, Error> {
        let factor = factor(rate)?;
        let taps = if factor == 1 {
            Vec::new()
        } else {
            quantise(&design(factor), 1.0)
        };
        Ok(Self {
            factor,
            line: vec![0; taps.len().saturating_sub(1)],
            taps,
            phase: 0,
        })
    }

    /// `rate / 8000`.
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Filters `input` and appends the 8 kHz samples to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.factor == 1 {
            out.extend_from_slice(input);
            return;
        }
        let history = self.taps.len() - 1;
        self.line.extend_from_slice(input);
        for i in 0..input.len() {
            // Keep the first of every `factor` inputs: with the
            // interpolator's phase 0 on the 8 kHz samples, a round trip is
            // a whole-sample delay.
            let keep = self.phase == 0;
            self.phase = (self.phase + 1) % self.factor;
            if !keep {
                continue;
            }
            // Newest sample at `history + i`; the filter is symmetric, so
            // the tap order does not matter.
            let window = &self.line[i..=history + i];
            let acc: i64 = window
                .iter()
                .zip(&self.taps)
                .map(|(&x, &h)| i64::from(x) * i64::from(h))
                .sum();
            out.push(to_sample(acc));
        }
        self.line.drain(..input.len());
    }
}

/// 8 kHz → `rate`. State carries across [`Interpolator::process`] calls.
#[derive(Debug, Clone)]
pub struct Interpolator {
    factor: usize,
    /// `phases[p][k]`: tap `k * factor + p` of the lowpass, times `factor`
    /// (the zeros take `factor - 1` of every `factor` samples' energy).
    phases: Vec<Vec<i32>>,
    /// Last `taps_per_phase - 1` inputs, then the current call's input.
    line: Vec<i16>,
}

impl Interpolator {
    /// For a codec running at `rate` Hz.
    pub fn new(rate: u32) -> Result<Self, Error> {
        let factor = factor(rate)?;
        if factor == 1 {
            return Ok(Self {
                factor,
                phases: Vec::new(),
                line: Vec::new(),
            });
        }
        let h = design(factor);
        // Each phase alone must have unity DC gain, or a constant input
        // comes out with a ripple at 8 kHz.
        let phases: Vec<Vec<i32>> = (0..factor)
            .map(|p| {
                let taps: Vec<f64> = h.iter().skip(p).step_by(factor).copied().collect();
                let sum: f64 = taps.iter().sum();
                let normalised: Vec<f64> = taps.iter().map(|t| t / sum).collect();
                quantise(&normalised, 1.0)
            })
            .collect();
        let longest = phases.iter().map(Vec::len).max().unwrap_or(1);
        Ok(Self {
            factor,
            line: vec![0; longest - 1],
            phases,
        })
    }

    /// `rate / 8000`.
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Filters 8 kHz `input` and appends `factor` samples per input to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.factor == 1 {
            out.extend_from_slice(input);
            return;
        }
        let history = self.line.len();
        self.line.extend_from_slice(input);
        out.reserve(input.len() * self.factor);
        for i in 0..input.len() {
            let newest = history + i;
            for phase in &self.phases {
                // y[n·L + p] = Σ_k h[k·L + p] · x[n − k]
                let acc: i64 = phase
                    .iter()
                    .enumerate()
                    .map(|(k, &h)| i64::from(self.line[newest - k]) * i64::from(h))
                    .sum();
                out.push(to_sample(acc));
            }
        }
        self.line.drain(..input.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, amplitude: f64, len: usize) -> Vec<i16> {
        (0..len)
            .map(|n| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * n as f64 / f64::from(rate)).sin())
                    .round() as i16
            })
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn db(ratio: f64) -> f64 {
        20.0 * ratio.log10()
    }

    /// Decimates `input` in uneven chunks (state across calls) and drops
    /// the filter's start-up transient.
    fn decimate(rate: u32, input: &[i16]) -> Vec<i16> {
        let mut d = Decimator::new(rate).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            d.process(chunk, &mut out);
        }
        out.split_off(64)
    }

    #[test]
    fn rates() {
        assert_eq!(factor(8000), Ok(1));
        assert_eq!(factor(16000), Ok(2));
        assert_eq!(factor(48000), Ok(6));
        for bad in [0, 11025, 44100, 56000] {
            assert_eq!(factor(bad), Err(Error::UnsupportedRate { rate: bad }));
            assert!(Decimator::new(bad).is_err());
            assert!(Interpolator::new(bad).is_err());
        }
        let d = Decimator::new(16000).unwrap();
        assert_eq!((d.factor(), d.taps.len()), (2, 73));
        assert_eq!(d.taps.iter().map(|&t| i64::from(t)).sum::<i64>(), ONE);
        let i = Interpolator::new(16000).unwrap();
        for phase in &i.phases {
            assert_eq!(phase.iter().map(|&t| i64::from(t)).sum::<i64>(), ONE);
        }
    }

    #[test]
    fn eight_khz_passes_through() {
        let x = tone(1000.0, 8000, 8000.0, 400);
        let mut out = Vec::new();
        Decimator::new(8000).unwrap().process(&x, &mut out);
        Interpolator::new(8000).unwrap().process(&x, &mut out);
        assert_eq!(out, [x.clone(), x].concat());
    }

    #[test]
    fn dc_gain_is_one() {
        for rate in [16000, 24000, 48000] {
            let out = decimate(rate, &vec![10_000; 4000]);
            assert_eq!(out.len(), 4000usize.div_ceil((rate / 8000) as usize) - 64);
            assert!(out.iter().all(|&s| (s - 10_000).abs() <= 2), "{rate}");

            let mut up = Vec::new();
            let mut i = Interpolator::new(rate).unwrap();
            i.process(&[-12_000; 1000], &mut up);
            assert_eq!(up.len(), 1000 * (rate / 8000) as usize);
            assert!(up[400..].iter().all(|&s| (s + 12_000).abs() <= 2), "{rate}");
        }
    }

    #[test]
    fn one_khz_passes_decimation() {
        let x = tone(1000.0, 16000, 10_000.0, 16000);
        let y = decimate(16000, &x);
        let gain = db(rms(&y) / rms(&x));
        assert!(gain.abs() < 0.2, "{gain} dB");
    }

    #[test]
    fn above_band_is_attenuated_30_db() {
        for rate in [16000u32, 48000] {
            for hz in [3900.0, 4000.0, 4500.0, 6000.0, 7900.0] {
                let x = tone(hz, rate, 20_000.0, rate as usize);
                let y = decimate(rate, &x);
                let gain = db(rms(&y).max(0.5) / rms(&x));
                assert!(gain <= -30.0, "{rate} Hz rate, {hz} Hz: {gain} dB");
            }
        }
    }

    #[test]
    fn interpolation_round_trip() {
        // 8 kHz voice band → 16 kHz → 8 kHz is the input, delayed by the two
        // filters' group delay (2 × 36 samples at 16 kHz = 36 at 8 kHz).
        let x: Vec<i16> = tone(1000.0, 8000, 8000.0, 4000)
            .iter()
            .zip(tone(300.0, 8000, 4000.0, 4000))
            .map(|(&a, b)| a + b)
            .collect();
        let mut up = Vec::new();
        let mut i = Interpolator::new(16000).unwrap();
        for chunk in x.chunks(160) {
            i.process(chunk, &mut up);
        }
        assert_eq!(up.len(), 8000);
        let mut back = Vec::new();
        let mut d = Decimator::new(16000).unwrap();
        for chunk in up.chunks(1024) {
            d.process(chunk, &mut back);
        }
        assert_eq!(back.len(), 4000);

        let (lag, err) = (0..80)
            .map(|lag| {
                let diff: Vec<i16> = x[200..3800]
                    .iter()
                    .zip(&back[200 + lag..3800 + lag])
                    .map(|(&a, &b)| a.saturating_sub(b))
                    .collect();
                (lag, rms(&diff))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert_eq!(lag, 36, "group delay");
        assert!(err < rms(&x) * 0.02, "error {err} vs signal {}", rms(&x));
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        // A full-scale square wave overshoots (Gibbs); the output must clip,
        // not wrap to the other sign.
        let x: Vec<i16> = (0..2000)
            .map(|n| if (n / 8) % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        let mut up = Vec::new();
        Interpolator::new(16000).unwrap().process(&x, &mut up);
        for (n, pair) in up.windows(2).enumerate().skip(100) {
            let jump = (i32::from(pair[0]) - i32::from(pair[1])).abs();
            assert!(jump < 40_000, "wrapped at {n}: {pair:?}");
        }
    }
}
