//! Repair: replaces a short stretch of sound, such as a click, with what the sound around it predicts.
//! A linear predictor (Burg's method) is learned on the sound that is not broken, then all broken frames
//! nearby are solved together as the values the predictor finds least surprising (least-squares
//! autoregressive interpolation, after Janssen, Veldhuis and Vries 1986).

/// How much sound on each side the predictor learns from: several periods of a low tone.
pub(crate) const CONTEXT_MS: f32 = 80.0;

/// The longest repair: past this the prediction has little to do with the sound.
pub(crate) const MAX_MS: f32 = 50.0;

/// How many past samples one prediction weighs: enough to follow a chord of a few tones.
const ORDER: usize = 128;

/// Fills the frames where `weights` is above zero, in the measure of the weight. Frames marked in `others`,
/// under the lane's other repairs, are broken too: not learned from, and solved along with the range, so
/// what the other repairs make never matters.
pub(crate) fn fill(sound: &mut [Vec<f32>; 2], weights: &[f32], others: &[bool]) {
    let broken = |i: usize| weights.get(i).is_some_and(|w| *w > 0.0) || others.get(i).copied().unwrap_or(false);
    for (channel, seed) in sound.iter_mut().zip([1, 2]) {
        let x: Vec<f64> = channel.iter().map(|&v| f64::from(v)).collect();
        let mut stretches: Vec<Vec<f64>> = vec![vec![]];
        for (i, &v) in x.iter().enumerate() {
            match (broken(i), stretches.last_mut()) {
                (false, Some(last)) => last.push(v),
                (true, Some(last)) if last.is_empty() => {}
                _ => stretches.push(vec![]),
            }
        }
        let known = stretches.iter().map(Vec::len).sum::<usize>();
        let (a, error_power) = burg(&stretches, ORDER.min(known / 4));
        let p = a.len().saturating_sub(1);
        // Each frame solved needs its predictions, p frames on either side, inside the buffer.
        let unknown: Vec<usize> = (p..x.len().saturating_sub(p)).filter(|&i| broken(i)).collect();
        let Some(filled) = solve(&x, &unknown, &a, error_power, seed) else {
            continue;
        };
        for (&i, v) in unknown.iter().zip(filled) {
            if let (Some(s), Some(&w)) = (channel.get_mut(i), weights.get(i)) {
                *s += w * (v as f32 - *s);
            }
        }
    }
}

/// The values at `unknown` (sorted) that make the predictor's errors over `x` smallest, plus random sound
/// shaped by the predictor and as large as its errors: alone, the best guess for noise such as rain or hiss
/// is near silence, while a tone is predicted with almost no error and stays as it is.
fn solve(x: &[f64], unknown: &[usize], a: &[f64], error_power: f64, seed: u64) -> Option<Vec<f64>> {
    let (&first, &last) = (unknown.first()?, unknown.last()?);
    let p = a.len().saturating_sub(1);
    // The error at frame n is the sum over k of a[k] x[n - k]. The sum of its squares over all frames is
    // smallest where M v = b, with M[i][j] = r(t_i - t_j) and b as below.
    let r: Vec<f64> = (0..=p)
        .map(|d| a.iter().zip(a.iter().skip(d)).map(|(u, v)| u * v).sum())
        .collect();
    let mut zeroed = x.to_vec();
    for &t in unknown {
        if let Some(v) = zeroed.get_mut(t) {
            *v = 0.0;
        }
    }
    let error_at = |n: usize| -> f64 {
        a.iter()
            .enumerate()
            .map(|(k, ak)| ak * n.checked_sub(k).and_then(|j| zeroed.get(j)).copied().unwrap_or(0.0))
            .sum()
    };
    let errors: Vec<f64> = (first..=last + p).map(error_at).collect();
    let b: Vec<f64> = unknown
        .iter()
        .map(|&t| {
            -a.iter()
                .zip(errors.iter().skip(t - first))
                .map(|(aj, e)| aj * e)
                .sum::<f64>()
        })
        .collect();
    // Cholesky: M = L Lᵀ. Unknowns more than p apart never meet in one prediction, so L is zero further
    // than p below its diagonal; row i holds the columns lo(i)..=i.
    let lo = |i: usize| i.saturating_sub(p);
    let mut l: Vec<Vec<f64>> = Vec::with_capacity(unknown.len());
    for (i, &ti) in unknown.iter().enumerate() {
        let mut row: Vec<f64> = Vec::with_capacity(i - lo(i) + 1);
        for (j, &tj) in unknown.iter().enumerate().take(i + 1).skip(lo(i)) {
            let other: &[f64] = if j < i { l.get(j)? } else { &row };
            let mut s = r.get(ti - tj).copied().unwrap_or(0.0);
            for k in lo(i).max(lo(j))..j {
                s -= row.get(k - lo(i)).copied().unwrap_or(0.0) * other.get(k - lo(j)).copied().unwrap_or(0.0);
            }
            let diagonal_j = other.last().copied().unwrap_or(1.0);
            if j < i {
                row.push(s / diagonal_j);
            } else if s > 0.0 {
                row.push(s.sqrt());
            } else {
                return None;
            }
        }
        l.push(row);
    }
    let diagonal = |i: usize| l.get(i).and_then(|r| r.last()).copied().unwrap_or(1.0);
    // L y = b.
    let mut y: Vec<f64> = Vec::with_capacity(b.len());
    for (i, bi) in b.iter().enumerate() {
        let sum: f64 = l
            .get(i)?
            .iter()
            .zip(y.iter().skip(lo(i)))
            .map(|(lik, yk)| lik * yk)
            .sum();
        y.push((bi - sum) / diagonal(i));
    }
    // Lᵀ v = y + noise: the best guess plus a draw of what the predictor leaves unexplained. Uniform between
    // -sqrt(3) and sqrt(3) has a power of 1.
    let scale = (3.0 * error_power.max(0.0)).sqrt();
    let mut state = seed;
    let rhs: Vec<f64> = y.iter().map(|yi| yi + scale * random(&mut state)).collect();
    let mut v = vec![0.0; rhs.len()];
    for i in (0..rhs.len()).rev() {
        let below: f64 = (i + 1..rhs.len().min(i + p + 1))
            .map(|k| {
                let lki = l.get(k).and_then(|r| r.get(i - lo(k))).copied().unwrap_or(0.0);
                lki * v.get(k).copied().unwrap_or(0.0)
            })
            .sum();
        if let Some(vi) = v.get_mut(i) {
            *vi = (rhs.get(i).copied().unwrap_or(0.0) - below) / diagonal(i);
        }
    }
    Some(v)
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
/// small as it can be, forwards and backwards, over each of the stretches; and the power of its errors.
fn burg(stretches: &[Vec<f64>], order: usize) -> (Vec<f64>, f64) {
    let mut a = vec![1.0];
    let total = stretches.iter().map(Vec::len).sum::<usize>().max(1);
    let mut error_power = stretches.iter().flatten().map(|v| v * v).sum::<f64>() / total as f64;
    // forward[i] pairs with backward[i]: the errors at sample n and at sample n - 1, in each stretch.
    let mut errors: Vec<(Vec<f64>, Vec<f64>)> = stretches
        .iter()
        .map(|x| {
            let forward = x.iter().skip(1).copied().collect();
            let backward = x.iter().take(x.len().saturating_sub(1)).copied().collect();
            (forward, backward)
        })
        .collect();
    for _ in 0..order {
        let pairs = errors.iter().flat_map(|(f, b)| f.iter().zip(b));
        let num: f64 = pairs.clone().map(|(f, b)| f * b).sum();
        let den: f64 = pairs.map(|(f, b)| f * f + b * b).sum();
        if den <= f64::MIN_POSITIVE {
            break;
        }
        let k = -2.0 * num / den;
        for (forward, backward) in &mut errors {
            let pairs = || forward.iter().zip(backward.iter());
            let next_forward = pairs().skip(1).map(|(f, b)| f + k * b).collect();
            let next_backward = pairs()
                .take(forward.len().saturating_sub(1))
                .map(|(f, b)| b + k * f)
                .collect();
            (*forward, *backward) = (next_forward, next_backward);
        }
        error_power *= 1.0 - k * k;
        a.push(0.0);
        a = a.iter().zip(a.iter().rev()).map(|(x, y)| x + k * y).collect();
    }
    (a, error_power)
}
