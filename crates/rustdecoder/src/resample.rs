//! Sample-rate conversion and channel mapping for audio output.
//!
//! [`Resampler`] is a streaming windowed-sinc interpolator: every output sample is the sum of
//! the input samples within `HALF_WIDTH` input samples of its position, weighted by a sinc
//! kernel under a 4-term Blackman-Harris window (about 92 dB of stop band). When the rate goes
//! down the kernel is stretched by the ratio, so it is also the low-pass that keeps the
//! content above the new Nyquist from folding back. With the default 16 taps per side a
//! 44.1 kHz to 48 kHz conversion costs 32 multiply-adds per output sample and channel. The
//! kernel is tabulated at 512 steps per input sample and read with linear interpolation (error
//! near 1e-6, below 16-bit resolution). The first output sample is at the first input sample's
//! time (history before it is zero), so no latency has to be compensated.
//!
//! [`map_channels`] converts between channel counts: mono to anything copies to the front
//! pair, stereo to mono averages, 5.1 (WAVE order L R C LFE Ls Rs) to stereo uses the usual
//! ITU weights (centre and surrounds at -3 dB, LFE dropped) scaled by 1/(1 + 2 x 0.707) so that
//! full-scale input cannot clip.

use std::f64::consts::PI;

/// Input samples on each side of the interpolation point at ratio 1.
pub const HALF_WIDTH: usize = 16;
const RES: usize = 512;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

/// 4-term Blackman-Harris, `x` in -1..1.
fn window(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let p = PI * (x + 1.0); // 0..2pi over the window
    0.35875 - 0.48829 * p.cos() + 0.14128 * (2.0 * p).cos() - 0.01168 * (3.0 * p).cos()
}

pub struct Resampler {
    channels: usize,
    /// Input samples per output sample.
    step: f64,
    /// Kernel half width in input samples (HALF_WIDTH, stretched when downsampling).
    half: f64,
    /// Kernel values at x = i / RES (the kernel is symmetric).
    table: Vec<f32>,
    /// Input frames not yet dropped, interleaved.
    buf: Vec<f32>,
    /// Position of the next output, in input frames relative to `buf`'s frame 0.
    pos: f64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        let ratio = out_rate as f64 / in_rate as f64;
        // cut-off a little under the lower of the two Nyquists, to leave a transition band
        let fc = if ratio < 1.0 { ratio * 0.97 } else { 1.0 };
        let half = HALF_WIDTH as f64 / fc;
        let n = (half * RES as f64).ceil() as usize + 2;
        let table = (0..n)
            .map(|i| {
                let x = i as f64 / RES as f64;
                (fc * sinc(fc * x) * window(x / half)) as f32
            })
            .collect();
        Resampler { channels, step: in_rate as f64 / out_rate as f64, half, table, buf: Vec::new(), pos: 0.0 }
    }

    pub fn is_identity(&self) -> bool {
        (self.step - 1.0).abs() < 1e-12
    }

    fn kernel(&self, x: f64) -> f32 {
        let f = x.abs() * RES as f64;
        let i = f as usize;
        match (self.table.get(i), self.table.get(i + 1)) {
            (Some(a), Some(b)) => a + (b - a) * (f - i as f64) as f32,
            _ => 0.0,
        }
    }

    /// Converts `input` (interleaved), appending to `out`. Keeps what it still needs of the
    /// input for the next call.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        self.buf.extend_from_slice(input);
        let frames = self.buf.len() / ch;
        let mut acc = vec![0f32; ch];
        // an output needs input up to pos + half
        while frames > 0 && self.pos + self.half < frames as f64 {
            let lo = (self.pos - self.half).ceil().max(0.0) as usize;
            let hi = ((self.pos + self.half).floor() as usize).min(frames - 1);
            acc.iter_mut().for_each(|a| *a = 0.0);
            for i in lo..=hi {
                let k = self.kernel(self.pos - i as f64);
                let base = i * ch;
                for c in 0..ch {
                    acc[c] += k * self.buf[base + c];
                }
            }
            out.extend_from_slice(&acc);
            self.pos += self.step;
        }
        // drop frames no later output can reach
        let keep_from = (self.pos - self.half).floor().max(0.0) as usize;
        if keep_from > 0 {
            self.buf.drain(..keep_from * ch);
            self.pos -= keep_from as f64;
        }
    }

    /// Outputs the tail (the input is padded with zeros so the last samples get their full
    /// kernel).
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.is_identity() {
            return;
        }
        let pad = vec![0f32; (self.half.ceil() as usize + 1) * self.channels];
        self.process(&pad, out);
        self.reset();
    }

    /// Forgets the input (after a seek).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0.0;
    }
}

/// Maps interleaved frames from `from` channels to `to` channels, appending to `out`.
pub fn map_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    out.reserve(input.len() / from * to);
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for f in input.chunks_exact(from) {
        match (from, to) {
            (1, _) => {
                // front pair; any further channels silent
                out.push(f[0]);
                if to > 1 {
                    out.push(f[0]);
                }
                out.extend(std::iter::repeat_n(0.0, to.saturating_sub(2)));
            }
            (_, 1) => out.push(f.iter().sum::<f32>() / from as f32),
            (6, 2) => {
                let g = 1.0 / (1.0 + 2.0 * H);
                out.push((f[0] + H * f[2] + H * f[4]) * g);
                out.push((f[1] + H * f[2] + H * f[5]) * g);
            }
            (8, 2) => {
                // L R C LFE Ls Rs Lb Rb
                let g = 1.0 / (1.0 + 3.0 * H);
                out.push((f[0] + H * f[2] + H * f[4] + H * f[6]) * g);
                out.push((f[1] + H * f[2] + H * f[5] + H * f[7]) * g);
            }
            _ => {
                // the channels both have in common, the rest silent (or dropped)
                for c in 0..to {
                    out.push(if c < from { f[c] } else { 0.0 });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, secs: f64, amp: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize).map(|i| (amp * (2.0 * PI * freq * i as f64 / rate as f64).sin()) as f32).collect()
    }

    /// Frequency from positive-going zero crossings, placed by linear interpolation between
    /// samples, over the middle of the signal.
    fn frequency(x: &[f32], rate: u32) -> f64 {
        let (a, b) = (x.len() / 10, x.len() * 9 / 10);
        let mut first = None;
        let mut last = 0.0;
        let mut n = 0;
        for i in a..b - 1 {
            if x[i] < 0.0 && x[i + 1] >= 0.0 {
                let t = i as f64 + (-x[i] as f64) / (x[i + 1] - x[i]) as f64;
                first.get_or_insert(t);
                last = t;
                n += 1;
            }
        }
        (n - 1) as f64 * rate as f64 / (last - first.unwrap())
    }

    fn rms(x: &[f32]) -> f64 {
        let (a, b) = (x.len() / 10, x.len() * 9 / 10);
        (x[a..b].iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / (b - a) as f64).sqrt()
    }

    #[test]
    fn sine_keeps_frequency_and_level() {
        for (ir, or) in [(44100u32, 48000u32), (48000, 44100), (96000, 48000), (22050, 48000)] {
            let input = sine(ir, 1000.0, 1.0, 0.5);
            let mut r = Resampler::new(ir, or, 1);
            let mut out = Vec::new();
            // in uneven chunks, the way a decoder delivers
            for c in input.chunks(1237) {
                r.process(c, &mut out);
            }
            r.finish(&mut out);
            let f = frequency(&out, or);
            let db = 20.0 * (rms(&out) / (0.5 / 2f64.sqrt())).log10();
            println!("{ir} -> {or}: {f:.3} Hz, level {db:+.4} dB, {} frames", out.len());
            assert!((f - 1000.0).abs() < 0.5, "{f}");
            assert!(db.abs() < 0.1, "{db}");
            assert!((out.len() as f64 - or as f64).abs() < 40.0);
        }
    }

    #[test]
    fn above_nyquist_is_removed_when_downsampling() {
        // 30 kHz at 96 kHz going to 48 kHz would alias to 18 kHz without the low-pass
        let input = sine(96000, 30000.0, 0.5, 0.5);
        let mut r = Resampler::new(96000, 48000, 1);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let db = 20.0 * (rms(&out) / (0.5 / 2f64.sqrt())).log10();
        assert!(db < -60.0, "{db}");
    }

    #[test]
    fn channel_maps() {
        let mut o = Vec::new();
        map_channels(&[1.0, 2.0], 1, 2, &mut o);
        assert_eq!(o, [1.0, 1.0, 2.0, 2.0]);
        o.clear();
        map_channels(&[1.0, 3.0], 2, 1, &mut o);
        assert_eq!(o, [2.0]);
        o.clear();
        // full scale everywhere stays within range
        map_channels(&[1.0; 6], 6, 2, &mut o);
        assert!(o.iter().all(|&x| x <= 1.0001 && x > 0.5), "{o:?}");
        o.clear();
        // centre only reaches both sides equally
        map_channels(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], 6, 2, &mut o);
        assert!((o[0] - o[1]).abs() < 1e-6 && o[0] > 0.2);
    }
}
