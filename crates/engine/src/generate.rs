//! Sound the editor makes itself: tones, noise and blips. A generated source keeps its settings, so it can be
//! changed later and made again for any length.

use crate::mix::db_to_amp;
use crate::{Error, Frames, Result, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wave {
    Sine,
    Triangle,
    Square,
    Saw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseColor {
    /// Even hiss: every frequency equally strong.
    White,
    /// Softer, like rain or wind: 3 dB less per octave up.
    Pink,
    /// A deep rumble: 6 dB less per octave up.
    Brown,
}

/// What to make. Levels are the loudest point in dB.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Generator {
    Tone {
        wave: Wave,
        hz: f32,
        level_db: f32,
    },
    /// `wide`: different noise left and right. `variation` picks another noise that sounds the same each time.
    Noise {
        color: NoiseColor,
        level_db: f32,
        wide: bool,
        variation: u32,
    },
    /// Short tones of `blip_ms`, one every `every_ms`.
    Blips {
        wave: Wave,
        hz: f32,
        blip_ms: f32,
        every_ms: f32,
        level_db: f32,
    },
}

/// The settings a generated source was made with, kept so it can be made again.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    pub generator: Generator,
    /// Which frame of the endless generated sound the source starts at.
    pub first: Frames,
    /// The loop length the sound was fitted to, so it repeats without a click; `None` without a loop.
    pub fitted: Option<Frames>,
}

/// Noise without a loop repeats after this long, which is too long to hear.
const NOISE_SECONDS: Frames = 30;
/// The longest sound made at once; it is held in memory like decoded audio.
const MAX_SECONDS: Frames = 3600;

impl Generator {
    /// One of each kind with its usual settings, in menu order.
    pub fn defaults() -> Vec<Generator> {
        vec![
            Generator::Tone {
                wave: Wave::Sine,
                hz: 440.0,
                level_db: -12.0,
            },
            Generator::Noise {
                color: NoiseColor::Pink,
                level_db: -12.0,
                wide: true,
                variation: 1,
            },
            Generator::Blips {
                wave: Wave::Sine,
                hz: 1000.0,
                blip_ms: 80.0,
                every_ms: 500.0,
                level_db: -12.0,
            },
        ]
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Generator::Tone { .. } => "Tone",
            Generator::Noise { .. } => "Noise",
            Generator::Blips { .. } => "Blips",
        }
    }

    /// A clip name that says what it sounds like.
    pub fn describe(&self) -> String {
        match *self {
            Generator::Tone { wave, hz, .. } => format!("{} {hz:.0} Hz", wave_name(wave)),
            Generator::Noise { color, .. } => format!("{} noise", color_name(color)),
            Generator::Blips { hz, every_ms, .. } => format!("Blips {hz:.0} Hz every {every_ms:.0} ms"),
        }
    }

    pub(crate) fn check(self) -> Result<Generator> {
        let within = |v: f32, r: std::ops::RangeInclusive<f32>| v.is_finite() && r.contains(&v);
        let problem = match self {
            Generator::Tone { hz, .. } | Generator::Blips { hz, .. } if !within(hz, 10.0..=20_000.0) => {
                "a tone must be between 10 Hz and 20 kHz"
            }
            Generator::Tone { level_db, .. }
            | Generator::Noise { level_db, .. }
            | Generator::Blips { level_db, .. }
                if !within(level_db, -60.0..=0.0) =>
            {
                "the level must be between -60 and 0 dB"
            }
            Generator::Blips { blip_ms, every_ms, .. }
                if !within(blip_ms, 2.0..=5000.0) || !within(every_ms, blip_ms..=10_000.0) =>
            {
                "a blip lasts 2 ms to 5 s and comes at most every 10 s, not before the last one ended"
            }
            _ => return Ok(self),
        };
        Err(Error::Invalid(problem.into()))
    }

    /// [`Generator::source`] after checking the settings and the length.
    pub(crate) fn make(self, rate: u32, loop_end: Option<Frames>, first: Frames, len: Frames) -> Result<Source> {
        let generator = self.check()?;
        if !(8000..=384_000).contains(&rate) {
            return Err(Error::Invalid(
                "the sample rate must be between 8000 and 384000 Hz".into(),
            ));
        }
        if len <= 0 || len > Frames::from(rate) * MAX_SECONDS {
            return Err(Error::Invalid("a generated sound lasts up to one hour".into()));
        }
        Ok(generator.source(rate, loop_end, first, len))
    }

    /// Makes `len` frames from frame `first` of the endless sound. With a loop it is fitted to repeat every
    /// `loop_end` frames: a whole number of waves, of blips, or the noise itself.
    pub(crate) fn source(self, rate: u32, loop_end: Option<Frames>, first: Frames, len: Frames) -> Source {
        let fitted = loop_end.filter(|l| *l > 0);
        let frames = first..first.saturating_add(len.max(0));
        let rate_f = f64::from(rate.max(1));
        let channels = match self {
            Generator::Tone { wave, hz, level_db } => {
                let amp = f64::from(db_to_amp(level_db));
                let tone = fitted_tone(f64::from(hz), rate_f, fitted);
                vec![frames.map(|k| (amp * tone.at(k, wave)) as f32).collect()]
            }
            Generator::Blips {
                wave,
                hz,
                blip_ms,
                every_ms,
                level_db,
            } => {
                let amp = f64::from(db_to_amp(level_db));
                let every = f64::from(every_ms) * rate_f / 1000.0;
                // A whole number of blips per loop.
                let every = fitted.map_or(every, |l| l as f64 / (l as f64 / every).round().max(1.0));
                let blip = (f64::from(blip_ms) * rate_f / 1000.0).min(every);
                // Short fades so a blip does not click.
                let fade = (rate_f * 0.002).min(blip / 2.0).max(1.0);
                let tone = fitted_tone(f64::from(hz), rate_f, None);
                let at = |k: Frames| {
                    let into = (k as f64).rem_euclid(every);
                    if into >= blip {
                        return 0.0;
                    }
                    let envelope = (into / fade).min((blip - into) / fade).min(1.0);
                    amp * envelope * tone.at_time(into, wave)
                };
                vec![frames.map(|k| at(k) as f32).collect()]
            }
            Generator::Noise {
                color,
                level_db,
                wide,
                variation,
            } => {
                let period = fitted.unwrap_or(Frames::from(rate) * NOISE_SECONDS).max(1);
                let sides: u64 = if wide { 2 } else { 1 };
                (0..sides)
                    .map(|side| {
                        let seed = u64::from(variation) * 2 + side;
                        let noise = noise_period(color, seed, usize::try_from(period).unwrap_or(1), rate_f);
                        let peak = noise.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-9);
                        let scale = db_to_amp(level_db) / peak;
                        frames
                            .clone()
                            .map(|k| {
                                let i = usize::try_from(k.rem_euclid(period)).unwrap_or(0);
                                noise.get(i).copied().unwrap_or(0.0) * scale
                            })
                            .collect()
                    })
                    .collect()
            }
        };
        Source {
            path: None,
            name: self.describe(),
            sample_rate: rate,
            channels,
            generated: Some(Generated {
                generator: self,
                first,
                fitted,
            }),
        }
    }
}

pub fn wave_name(wave: Wave) -> &'static str {
    match wave {
        Wave::Sine => "Sine",
        Wave::Triangle => "Triangle",
        Wave::Square => "Square",
        Wave::Saw => "Saw",
    }
}

pub fn color_name(color: NoiseColor) -> &'static str {
    match color {
        NoiseColor::White => "White",
        NoiseColor::Pink => "Pink",
        NoiseColor::Brown => "Brown",
    }
}

/// A tone of a fixed frequency. Fitted to a loop it has a whole number of waves per loop, and the phase is
/// counted in whole numbers so it stays exact however far into the sound.
struct Tone {
    /// Waves per `per` frames.
    waves: f64,
    per: f64,
}

fn fitted_tone(hz: f64, rate: f64, loop_end: Option<Frames>) -> Tone {
    match loop_end {
        Some(l) => Tone {
            waves: (hz * l as f64 / rate).round().max(1.0),
            per: l as f64,
        },
        // Counted per 100 s, a whole number of waves for any frequency to 0.01 Hz.
        None => Tone {
            waves: (hz * 100.0).round().max(1.0),
            per: rate * 100.0,
        },
    }
}

impl Tone {
    /// The frequency actually played, in Hz.
    fn hz(&self, rate: f64) -> f64 {
        self.waves * rate / self.per
    }

    fn at(&self, k: Frames, wave: Wave) -> f64 {
        // Whole loops do not change the phase; leaving them out keeps the numbers small.
        let k = (k as f64).rem_euclid(self.per);
        shape(wave, (self.waves * k / self.per).fract(), self.waves / self.per)
    }

    /// At `frames` after the tone started, which need not be whole.
    fn at_time(&self, frames: f64, wave: Wave) -> f64 {
        shape(wave, (self.waves * frames / self.per).fract(), self.waves / self.per)
    }
}

/// One wave at `phase` (0..1) that moves `step` per frame. Square and saw jump, which would add harsh tones
/// above what the rate can hold; a small correction rounds each jump off (the "polyBLEP" method).
fn shape(wave: Wave, phase: f64, step: f64) -> f64 {
    let jump = |t: f64| {
        if t < step {
            let t = t / step;
            2.0 * t - t * t - 1.0
        } else if t > 1.0 - step {
            let t = (t - 1.0) / step;
            t * t + 2.0 * t + 1.0
        } else {
            0.0
        }
    };
    match wave {
        Wave::Sine => (std::f64::consts::TAU * phase).sin(),
        Wave::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
        Wave::Saw => 2.0 * phase - 1.0 - jump(phase),
        Wave::Square => {
            let level = if phase < 0.5 { 1.0 } else { -1.0 };
            level + jump(phase) - jump((phase + 0.5).fract())
        }
    }
}

/// `len` frames of noise whose end runs straight into its start. Coloured noise is filtered white noise; the
/// filter runs over the noise twice and only the second pass is kept, so it starts with the memory the end
/// leaves and the seam does not jump.
fn noise_period(color: NoiseColor, seed: u64, len: usize, rate: f64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    let white: Vec<f64> = (0..len).map(|_| random(&mut state)).collect();
    let mut out = vec![0.0f32; len];
    match color {
        NoiseColor::White => out.iter_mut().zip(&white).for_each(|(o, w)| *o = *w as f32),
        NoiseColor::Pink => {
            // Three smoothing filters of different speeds add up to about 3 dB less per octave: Paul Kellet's
            // public-domain recipe.
            let mut b = [0.0f64; 3];
            for pass in 0..2 {
                for (o, w) in out.iter_mut().zip(&white) {
                    b[0] = 0.99765 * b[0] + w * 0.099_046;
                    b[1] = 0.963 * b[1] + w * 0.296_516_4;
                    b[2] = 0.57 * b[2] + w * 1.052_691_3;
                    if pass == 1 {
                        *o = (b[0] + b[1] + b[2] + w * 0.1848) as f32;
                    }
                }
            }
        }
        NoiseColor::Brown => {
            // One slow smoothing filter: 6 dB less per octave above 20 Hz.
            let keep = (-std::f64::consts::TAU * 20.0 / rate).exp();
            let mut b = 0.0;
            for pass in 0..2 {
                for (o, w) in out.iter_mut().zip(&white) {
                    b = keep * b + (1.0 - keep) * w;
                    if pass == 1 {
                        *o = b as f32;
                    }
                }
            }
        }
    }
    out
}

/// Even between -1 and 1. A fixed recipe ("SplitMix64"), so the same seed gives the same noise everywhere.
fn random(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

/// The frequency a tone plays at: with a loop, adjusted to a whole number of waves per loop.
pub fn played_hz(hz: f32, rate: u32, loop_end: Option<Frames>) -> f32 {
    let rate = f64::from(rate.max(1));
    fitted_tone(f64::from(hz), rate, loop_end.filter(|l| *l > 0)).hz(rate) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Session, render};

    const RATE: u32 = 48_000;

    fn samples(g: Generator, loop_end: Option<Frames>, first: Frames, len: Frames) -> Vec<Vec<f32>> {
        g.make(RATE, loop_end, first, len).unwrap().channels
    }

    #[test]
    fn with_a_loop_tone_blips_and_noise_repeat_exactly_once_per_loop() {
        let l = 47_000;
        let tone = Generator::Tone {
            wave: Wave::Saw,
            hz: 440.0,
            level_db: -6.0,
        };
        let blips = Generator::Blips {
            wave: Wave::Sine,
            hz: 1000.0,
            blip_ms: 50.0,
            every_ms: 500.0,
            level_db: -6.0,
        };
        let noise = Generator::Noise {
            color: NoiseColor::Pink,
            level_db: -6.0,
            wide: true,
            variation: 3,
        };
        for g in [tone, blips, noise] {
            for channel in samples(g, Some(l), -5000, 3 * l) {
                let differs = (0..2 * l as usize).map(|k| (channel[k] - channel[k + l as usize]).abs());
                assert!(differs.fold(0.0, f32::max) < 1e-5, "{g:?}");
            }
        }
        // 431 whole waves in the loop instead of 430.8.
        assert!((played_hz(440.0, RATE, Some(l)) - 440.17).abs() < 0.01);
    }

    #[test]
    fn brown_noise_has_no_jump_at_the_loop_seam() {
        let l = 4800;
        let brown = Generator::Noise {
            color: NoiseColor::Brown,
            level_db: 0.0,
            wide: false,
            variation: 1,
        };
        let s = &samples(brown, Some(l), 0, l + 1)[0];
        let steps: Vec<f32> = s.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = steps.iter().sum::<f32>() / steps.len() as f32;
        let seam = (s[l as usize] - s[l as usize - 1]).abs();
        assert!(seam < 4.0 * mean, "seam {seam}, mean step {mean}");
    }

    fn generated_session() -> (Session, crate::ClipId) {
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        let generator = Generator::Noise {
            color: NoiseColor::White,
            level_db: -6.0,
            wide: false,
            variation: 1,
        };
        // An empty project takes 48 kHz with its first generated clip.
        let len = Frames::from(RATE);
        s.execute(Command::AddGeneratedClip {
            track: Some(track),
            generator,
            start: len,
            len,
        })
        .unwrap();
        let clip = s.project().tracks[0].clips[0].id;
        (s, clip)
    }

    #[test]
    fn dragging_a_generated_clip_out_makes_more_and_keeps_what_was_there() {
        let (mut s, clip) = generated_session();
        let rate = Frames::from(s.project().sample_rate);
        let before = render(s.project(), rate, rate as usize);
        s.execute(Command::TrimClip {
            clip,
            start: rate / 2,
            end: 3 * rate,
        })
        .unwrap();
        let c = &s.project().tracks[0].clips[0];
        assert_eq!((c.offset, c.len, c.source.frames()), (0, 5 * rate / 2, 5 * rate / 2));
        assert_eq!(render(s.project(), rate, rate as usize), before);
        let new = render(s.project(), rate / 2, (rate / 2) as usize);
        assert!(new[0].iter().any(|v| v.abs() > 0.1), "sound in the part dragged out");
    }

    #[test]
    fn new_settings_rename_the_track_unless_its_name_was_typed_in() {
        let mut s = Session::default();
        let tone = Generator::Tone {
            wave: Wave::Sine,
            hz: 440.0,
            level_db: -12.0,
        };
        let add = Command::AddGeneratedClip {
            track: None,
            generator: tone,
            start: 0,
            len: 1000,
        };
        s.execute(add.clone()).unwrap();
        s.execute(add).unwrap();
        let typed = s.project().tracks[1].id;
        s.execute(Command::RenameTrack {
            track: typed,
            name: "Alarm".into(),
        })
        .unwrap();
        let higher = Generator::Tone {
            wave: Wave::Sine,
            hz: 880.0,
            level_db: -12.0,
        };
        for t in 0..2 {
            let clip = s.project().tracks[t].clips[0].id;
            s.execute(Command::SetGenerator {
                clip,
                generator: higher,
            })
            .unwrap();
        }
        let names: Vec<_> = s.project().tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Sine 880 Hz", "Alarm"]);
    }

    #[test]
    fn a_loop_or_rate_change_makes_the_sound_again() {
        let (mut s, _) = generated_session();
        s.execute(Command::SetLoopEnd { at: Some(120_000) }).unwrap();
        let fitted = |s: &Session| s.project().tracks[0].clips[0].source.generated.as_ref().unwrap().fitted;
        assert_eq!(fitted(&s), Some(120_000));
        s.execute(Command::SetSampleRate { rate: 24_000 }).unwrap();
        let source = &s.project().tracks[0].clips[0].source;
        assert_eq!((source.sample_rate, fitted(&s)), (24_000, s.project().loop_end));
    }
}
