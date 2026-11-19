//! Repair: replaces a short stretch of sound, such as a click, with what the sound around it predicts.
//! A linear predictor (Burg's method) is learned on the sound before the gap and carried forward, another
//! on the sound after it and carried backward, and the two are crossfaded across the gap.

/// How much sound on each side the predictors learn from: several periods of a low tone.
pub(crate) const CONTEXT_MS: f32 = 80.0;

/// How many past samples one prediction weighs: enough to follow a chord of a few tones.
const ORDER: usize = 128;

/// Fills the frames where `weights` is above zero from the sound before and after them, in the measure of
/// the weight. With no sound on either side, nothing changes.
pub(crate) fn fill(sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    let Some(first) = weights.iter().position(|w| *w > 0.0) else {
        return;
    };
    let last = weights.iter().rposition(|w| *w > 0.0).unwrap_or(first);
    for channel in sound {
        let len = channel.len();
        let (gap_start, gap_end) = (first.min(len), (last + 1).min(len));
        let gap = gap_end - gap_start;
        let as_f64 = |s: &[f32]| s.iter().map(|&v| f64::from(v)).collect::<Vec<f64>>();
        let before = as_f64(channel.get(..gap_start).unwrap_or(&[]));
        let mut after = as_f64(channel.get(gap_end..).unwrap_or(&[]));
        after.reverse();
        let forward = predict(&before, gap, 1);
        let mut backward = predict(&after, gap, 2);
        backward.reverse();
        let (has_before, has_after) = (before.len() > ORDER, after.len() > ORDER);
        if !has_before && !has_after {
            continue;
        }
        let gap_samples = channel.iter_mut().skip(gap_start).take(gap);
        let parts = forward.iter().zip(&backward).zip(weights.iter().skip(gap_start));
        for (j, (s, ((f, b), w))) in gap_samples.zip(parts).enumerate() {
            // Each side is trusted most near its own edge.
            let t = (j as f64 + 0.5) / gap as f64;
            let from_before = match (has_before, has_after) {
                (true, true) => 0.5 + 0.5 * (std::f64::consts::PI * t).cos(),
                (true, false) => 1.0,
                _ => 0.0,
            };
            let wet = (f * from_before + b * (1.0 - from_before)) as f32;
            *s += w * (wet - *s);
        }
    }
}

/// The `n` samples that would follow `history`, from a predictor learned on it. Each prediction gets random
/// sound as large as the predictor's errors on `history`: alone, a prediction of noise such as rain or hiss
/// dies away and leaves a hole, while a tone is predicted with almost no error and stays as it is.
fn predict(history: &[f64], n: usize, seed: u64) -> Vec<f64> {
    let (coefficients, error_power) = burg(history, ORDER.min(history.len() / 2));
    // Uniform between -sqrt(3) and sqrt(3) has a power of 1.
    let scale = (3.0 * error_power).sqrt();
    let mut state = seed;
    let mut known = history.to_vec();
    for _ in 0..n {
        let predicted = -coefficients
            .iter()
            .skip(1)
            .zip(known.iter().rev())
            .map(|(a, x)| a * x)
            .sum::<f64>();
        known.push(predicted + scale * random(&mut state));
    }
    known.split_off(history.len())
}

/// A number between -1 and 1 (splitmix64), the same each time for the same seed, so every mixdown of the
/// project sounds the same.
pub(crate) fn random(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

/// Burg's method: the predictor `a` (with `a[0]` = 1) for which the sum over k of `a[k] * x[n - k]` is as
/// small as it can be, forwards and backwards, and the power of its errors. Its predictions die away rather
/// than grow.
fn burg(x: &[f64], order: usize) -> (Vec<f64>, f64) {
    let mut a = vec![1.0];
    let mut error_power = x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64;
    // forward[i] pairs with backward[i]: the errors at sample n and at sample n - 1.
    let mut forward: Vec<f64> = x.iter().skip(1).copied().collect();
    let mut backward: Vec<f64> = x.iter().take(x.len().saturating_sub(1)).copied().collect();
    for _ in 0..order {
        let num: f64 = forward.iter().zip(&backward).map(|(f, b)| f * b).sum();
        let den: f64 = forward.iter().zip(&backward).map(|(f, b)| f * f + b * b).sum();
        if den <= f64::MIN_POSITIVE {
            break;
        }
        let k = -2.0 * num / den;
        let pairs = || forward.iter().zip(&backward);
        let next_forward = pairs().skip(1).map(|(f, b)| f + k * b).collect();
        let next_backward = pairs()
            .take(forward.len().saturating_sub(1))
            .map(|(f, b)| b + k * f)
            .collect();
        (forward, backward) = (next_forward, next_backward);
        error_power *= 1.0 - k * k;
        a.push(0.0);
        a = a.iter().zip(a.iter().rev()).map(|(x, y)| x + k * y).collect();
    }
    (a, error_power)
}
