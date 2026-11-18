//! Pitch shift that keeps the length. A phase vocoder stretches the sound in time by the pitch ratio, then
//! resampling plays it that much faster, back to its length and at the new pitch. Phase locking (Laroche and
//! Dolson, "Improved phase vocoder time-scale modification of audio", 1999) keeps the partials of a tone
//! together, which is what makes a plain phase vocoder sound hollow.

use std::f64::consts::TAU;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// About 85 ms as a power of two: long enough to tell apart the partials of low notes, short enough not to smear
/// much in time. 4096 at 48 kHz.
pub(crate) fn window_len(rate: u32) -> usize {
    let target = (f64::from(rate) * 0.085).max(1.0);
    1usize << (target.log2().round().clamp(8.0, 14.0) as u32)
}

/// Moves the pitch of `sound` by `semitones`, keeping its timing. `weights` says how much of the shifted sound
/// each frame gets.
pub(crate) fn shift(semitones: f32, rate: u32, sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    let ratio = 2f64.powf(f64::from(semitones) / 12.0);
    let frames = sound[0].len().min(sound[1].len());
    if semitones == 0.0 || frames == 0 {
        return;
    }
    let stretched = stretch(sound, frames, ratio, window_len(rate));
    // On failure the sound stays as it was, like an effect that is off.
    let Ok(wet) = crate::io::speed_up(&stretched, stretched[0].len(), ratio) else {
        return;
    };
    for (channel, wet) in sound.iter_mut().zip(&wet) {
        for ((s, w), weight) in channel.iter_mut().zip(wet.iter().chain(std::iter::repeat(&0.0))).zip(weights) {
            *s += weight * (w - *s);
        }
    }
}

fn principal(phase: f64) -> f64 {
    phase - TAU * (phase / TAU).round()
}

/// The sound `ratio` times as long at the same pitch. Both sides get the same phase changes, taken from their
/// sum, so the stereo picture stays in place.
fn stretch(sound: &[Vec<f32>; 2], frames: usize, ratio: f64, n: usize) -> [Vec<f32>; 2] {
    let (hop, half, bins) = (n / 4, n / 2, n / 2 + 1);
    let out_len = (frames as f64 * ratio).round() as usize;
    let window: Vec<f32> = (0..n)
        .map(|i| (0.5 - 0.5 * (TAU * i as f64 / n as f64).cos()) as f32)
        .collect();
    let mut planner = RealFftPlanner::<f32>::new();
    let (forward, inverse) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut grain = forward.make_input_vec();
    let mut spectra = [forward.make_output_vec(), forward.make_output_vec()];
    // Room for half a window on each side; frame `i` of the result is `out[i + half]`.
    let mut out = [vec![0.0f32; out_len + n], vec![0.0f32; out_len + n]];
    let mut norm = vec![0.0f32; out_len + n];
    let mut last_phase = vec![0.0f64; bins];
    let mut last_made = vec![0.0f64; bins];
    let mut rotation = vec![0.0f64; bins];
    let mut last_in: Option<i64> = None;
    let mut magnitude = vec![0.0f64; bins];
    let mut phase = vec![0.0f64; bins];
    let mut peaks: Vec<usize> = vec![];
    for at_out in (0..=out_len + half).step_by(hop) {
        let at_in = (at_out as f64 / ratio).round() as i64;
        for (channel, spectrum) in sound.iter().zip(&mut spectra) {
            for (i, (g, w)) in grain.iter_mut().zip(&window).enumerate() {
                let t = at_in + i as i64 - half as i64;
                *g = usize::try_from(t).ok().and_then(|t| channel.get(t)).copied().unwrap_or(0.0) * w;
            }
            if forward.process(&mut grain, spectrum).is_err() {
                return [sound[0].clone(), sound[1].clone()];
            }
        }
        let [left, right] = &spectra;
        for (k, (l, r)) in left.iter().zip(right).enumerate() {
            let sum = l + r;
            if let (Some(m), Some(p)) = (magnitude.get_mut(k), phase.get_mut(k)) {
                *m = f64::from(sum.norm());
                *p = f64::from(sum.arg());
            }
        }
        match last_in {
            None => rotation.fill(0.0),
            Some(last) => {
                let gone = (at_in - last).max(1) as f64;
                find_peaks(&magnitude, &mut peaks);
                // A peak's phase moves on at its own frequency; the bins around it keep their offset to it.
                let turn_of = |k: usize| {
                    let bin = TAU * k as f64 / n as f64;
                    let (p, lp, lm) = (at(&phase, k), at(&last_phase, k), at(&last_made, k));
                    let freq = bin + principal(p - lp - bin * gone) / gone;
                    lm + freq * hop as f64 - p
                };
                if peaks.is_empty() {
                    for (k, r) in rotation.iter_mut().enumerate() {
                        *r = turn_of(k);
                    }
                } else {
                    let turns: Vec<f64> = peaks.iter().map(|&k| turn_of(k)).collect();
                    // Bins up to the quietest point between two peaks follow the lower one.
                    let ends: Vec<usize> = peaks.windows(2).map(|w| lowest_between(&magnitude, w)).collect();
                    let mut region = 0;
                    for (k, r) in rotation.iter_mut().enumerate() {
                        while ends.get(region).is_some_and(|&end| k >= end) {
                            region += 1;
                        }
                        *r = turns.get(region).copied().unwrap_or(0.0);
                    }
                }
            }
        }
        last_in = Some(at_in);
        for k in 0..bins {
            if let (Some(lp), Some(lm)) = (last_phase.get_mut(k), last_made.get_mut(k)) {
                *lp = at(&phase, k);
                *lm = at(&phase, k) + at(&rotation, k);
            }
        }
        for (spectrum, out) in spectra.iter_mut().zip(&mut out) {
            for (c, r) in spectrum.iter_mut().zip(&rotation) {
                *c *= Complex::from_polar(1.0, *r as f32);
            }
            // A real signal has no imaginary part at 0 Hz and at the Nyquist frequency.
            for k in [0, bins - 1] {
                if let Some(c) = spectrum.get_mut(k) {
                    c.im = 0.0;
                }
            }
            if inverse.process(spectrum, &mut grain).is_err() {
                return [sound[0].clone(), sound[1].clone()];
            }
            let scale = 1.0 / n as f32;
            for ((o, g), w) in out.iter_mut().skip(at_out).zip(&grain).zip(&window) {
                *o += g * w * scale;
            }
        }
        for (s, w) in norm.iter_mut().skip(at_out).zip(&window) {
            *s += w * w;
        }
    }
    out.map(|channel| {
        channel
            .iter()
            .zip(&norm)
            .skip(half)
            .take(out_len)
            .map(|(s, w)| s / w.max(0.1))
            .collect()
    })
}

fn at(values: &[f64], k: usize) -> f64 {
    values.get(k).copied().unwrap_or(0.0)
}

/// Bins louder than the two on each side: the partials.
fn find_peaks(magnitude: &[f64], peaks: &mut Vec<usize>) {
    let loudest = magnitude.iter().fold(0.0f64, |m, v| m.max(*v));
    peaks.clear();
    peaks.extend((0..magnitude.len()).filter(|&k| {
        let m = at(magnitude, k);
        m > loudest * 1e-6
            && (k.saturating_sub(2)..(k + 3).min(magnitude.len())).all(|j| j == k || at(magnitude, j) < m)
    }));
}

/// The quietest bin between two peaks, from the first past the lower peak.
fn lowest_between(magnitude: &[f64], peaks: &[usize]) -> usize {
    let (a, b) = match peaks {
        [a, b] => (*a, *b),
        _ => return 0,
    };
    (a + 1..=b)
        .min_by(|&x, &y| at(magnitude, x).total_cmp(&at(magnitude, y)))
        .unwrap_or(b)
}

