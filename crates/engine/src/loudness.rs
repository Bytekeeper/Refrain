//! Loudness as people hear it, measured after ITU-R BS.1770, for loudness normalize.

use crate::effects::Biquad;

/// Integrated loudness in LUFS after ITU-R BS.1770: the sound is weighted like the ear hears it, its power
/// taken in 400 ms blocks that overlap by 75 %, and blocks of near silence left out so pauses do not count.
/// None when everything is that quiet.
pub(crate) fn loudness(audio: &[Vec<f32>; 2], rate: u32) -> Option<f64> {
    let len = audio[0].len();
    // Running sum of the weighted power of both channels, so each block's power is one subtraction.
    let mut sums = vec![0.0f64; len + 1];
    for channel in audio {
        let mut weighted = channel.clone();
        for (biquad, mut memory) in k_weighting(rate).iter().zip([[0.0; 4]; 2]) {
            biquad.run(&mut memory, &mut weighted);
        }
        let mut total = 0.0;
        for (sum, s) in sums.iter_mut().skip(1).zip(&weighted) {
            total += f64::from(*s) * f64::from(*s);
            *sum += total;
        }
    }
    // A range shorter than a block is measured as one block.
    let block = (rate as usize * 4 / 10).clamp(1, len.max(1));
    let powers: Vec<f64> = (0..)
        .map(|i| i * (block / 4).max(1))
        .take_while(|start| start + block <= len)
        .filter_map(|start| Some((sums.get(start + block)? - sums.get(start)?) / block as f64))
        .collect();
    let lufs = |power: f64| -0.691 + 10.0 * power.max(1e-20).log10();
    let mean = |powers: &[f64]| powers.iter().sum::<f64>() / powers.len().max(1) as f64;
    let audible: Vec<f64> = powers.into_iter().filter(|&p| lufs(p) > -70.0).collect();
    if audible.is_empty() {
        return None;
    }
    let gate = lufs(mean(&audible)) - 10.0;
    let kept: Vec<f64> = audible.iter().copied().filter(|&p| lufs(p) > gate).collect();
    Some(lufs(mean(&kept)))
}

/// The two filters of BS.1770's weighting: a shelf that lifts the highs by 4 dB, as the head does, then a
/// high-pass that ignores what is too low to hear well. The standard gives them for 48 kHz; these are the
/// same filters for any rate.
fn k_weighting(rate: u32) -> [Biquad; 2] {
    let k = |hz: f64| (std::f64::consts::PI * hz / f64::from(rate.max(1))).tan();
    let (k1, q1) = (k(1_681.974_450_955_533), 0.707_175_236_955_419_6);
    let vh = 10f64.powf(3.999_843_853_973_347 / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k1 / q1 + k1 * k1;
    let shelf = Biquad {
        b: [
            (vh + vb * k1 / q1 + k1 * k1) / a0,
            2.0 * (k1 * k1 - vh) / a0,
            (vh - vb * k1 / q1 + k1 * k1) / a0,
        ],
        a: [2.0 * (k1 * k1 - 1.0) / a0, (1.0 - k1 / q1 + k1 * k1) / a0],
    };
    let (k2, q2) = (k(38.135_470_876_024_44), 0.500_327_037_323_877_3);
    let a0 = 1.0 + k2 / q2 + k2 * k2;
    let high_pass = Biquad {
        b: [1.0, -2.0, 1.0],
        a: [2.0 * (k2 * k2 - 1.0) / a0, (1.0 - k2 / q2 + k2 * k2) / a0],
    };
    [shelf, high_pass]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_weighting_matches_the_standard_at_48_khz() {
        // The coefficients printed in ITU-R BS.1770-4, tables 1 and 2.
        let [shelf, high_pass] = k_weighting(48_000);
        let close = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-8);
        assert!(close(
            &shelf.b,
            &[1.535_124_859_586_97, -2.691_696_189_406_38, 1.198_392_810_852_85]
        ));
        assert!(close(&shelf.a, &[-1.690_659_293_182_41, 0.732_480_774_215_85]));
        assert!(close(&high_pass.a, &[-1.990_047_454_833_98, 0.990_072_250_366_21]));
    }

    #[test]
    fn a_full_scale_tone_at_997_hz_measures_minus_3_lufs_per_channel() {
        // The standard's check: this tone in one channel reads -3.01 LUFS; in both it is twice the power.
        let rate = 48_000;
        let tone: Vec<f32> = (0..rate * 2)
            .map(|i| (i as f32 * 997.0 * std::f32::consts::TAU / rate as f32).sin())
            .collect();
        let one = loudness(&[tone.clone(), vec![0.0; tone.len()]], rate).unwrap();
        assert!((one + 3.01).abs() < 0.05, "{one}");
        let both = loudness(&[tone.clone(), tone.clone()], rate).unwrap();
        assert!(both.abs() < 0.05, "{both}");
        // A pause does not make it much quieter: its silent blocks are left out, only the blocks across
        // the change from tone to silence count, with part of the power.
        let mut paused = tone.clone();
        paused.extend(vec![0.0; tone.len()]);
        let with_pause = loudness(&[paused.clone(), paused], rate).unwrap();
        assert!(with_pause < 0.0 && with_pause > -0.5, "{with_pause}");
        assert_eq!(loudness(&[vec![0.0; 1000], vec![0.0; 1000]], rate), None);
    }
}
