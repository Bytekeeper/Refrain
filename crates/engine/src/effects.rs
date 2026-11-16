//! Effects in lanes: each lane is one effect applied in its time ranges, lanes in order from the top.

use crate::mix::{db_to_amp, mix_upto, render_track};
use crate::{Error, Frames, Project, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaneId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RangeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Sets the gain of each range so its loudest sample reaches `peak_db`.
    Normalize { peak_db: f32 },
    /// Lets through what is above `hz`: removes rumble and hum. `steep` falls 24 dB per octave instead of 12.
    HighPass { hz: f32, steep: bool },
    /// Lets through what is below `hz`: removes hiss and harshness.
    LowPass { hz: f32, steep: bool },
}

impl Effect {
    /// One of each kind with its usual settings, in menu order.
    pub fn defaults() -> Vec<Effect> {
        vec![
            Effect::Normalize { peak_db: -1.0 },
            Effect::HighPass { hz: 80.0, steep: false },
            Effect::LowPass {
                hz: 8000.0,
                steep: false,
            },
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Effect::Normalize { .. } => "Normalize",
            Effect::HighPass { .. } => "High-pass",
            Effect::LowPass { .. } => "Low-pass",
        }
    }

    /// How long before a range the effect already changes the sound.
    pub fn lead_in(&self, _rate: u32) -> Frames {
        match self {
            Effect::Normalize { .. } | Effect::HighPass { .. } | Effect::LowPass { .. } => 0,
        }
    }

    /// How long after a range the effect still sounds.
    pub fn tail(&self, _rate: u32) -> Frames {
        match self {
            Effect::Normalize { .. } | Effect::HighPass { .. } | Effect::LowPass { .. } => 0,
        }
    }

    /// How much audio before a frame the effect needs to compute it: a filter's memory of the past fades to
    /// -80 dB in this time. Rendering starts this much earlier, so playback in pieces sounds like one render.
    pub(crate) fn warmup(&self, rate: u32) -> Frames {
        let Some((hz, stages)) = self.filter() else { return 0 };
        let w0 = std::f64::consts::TAU * f64::from(cutoff(hz, rate)) / f64::from(rate.max(1));
        // The slowest stage decays by exp(-w0 / (2 q)) per frame.
        let q = stages.iter().copied().fold(0.0f64, f64::max);
        let frames = 9.21 * 2.0 * q / w0.max(1e-9);
        (frames.ceil() as Frames)
            .saturating_mul(stages.len() as Frames)
            .min(Frames::from(rate) * 2)
    }

    /// Cutoff and the Q of each stage, for the filters.
    fn filter(&self) -> Option<(f32, &'static [f64])> {
        // Butterworth: flat until the cutoff; the steep one is two stages.
        const GENTLE: &[f64] = &[std::f64::consts::FRAC_1_SQRT_2];
        const STEEP: &[f64] = &[0.541_196_1, 1.306_563];
        match *self {
            Effect::HighPass { hz, steep } | Effect::LowPass { hz, steep } => {
                Some((hz, if steep { STEEP } else { GENTLE }))
            }
            Effect::Normalize { .. } => None,
        }
    }

    fn check(self) -> Result<Effect> {
        match self {
            Effect::Normalize { peak_db } if peak_db.is_finite() && (-60.0..=0.0).contains(&peak_db) => Ok(self),
            Effect::Normalize { .. } => Err(Error::Invalid("normalize goes to a peak between -60 and 0 dB".into())),
            Effect::HighPass { hz, .. } | Effect::LowPass { hz, .. }
                if hz.is_finite() && (10.0..=40_000.0).contains(&hz) =>
            {
                Ok(self)
            }
            Effect::HighPass { .. } | Effect::LowPass { .. } => Err(Error::Invalid(
                "a filter's frequency must be between 10 Hz and 40 kHz".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectRange {
    pub id: RangeId,
    pub start: Frames,
    pub end: Frames,
    /// Measured from the audio under the range whenever the project changes; for normalize the gain.
    pub gain: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectLane {
    pub id: LaneId,
    pub effect: Effect,
    pub enabled: bool,
    /// Sorted by start, not overlapping; with a loop each starts in `0..loop_end` and may run past it.
    pub ranges: Vec<EffectRange>,
}

impl EffectLane {
    pub fn new(id: LaneId, effect: Effect) -> Result<EffectLane> {
        Ok(EffectLane {
            id,
            effect: effect.check()?,
            enabled: true,
            ranges: vec![],
        })
    }

    pub fn set_effect(&mut self, effect: Effect) -> Result<()> {
        self.effect = effect.check()?;
        Ok(())
    }
}

/// The fades between the plain and the processed sound at both edges of a range, so it never clicks.
fn edge_frames(rate: u32, len: Frames) -> Frames {
    (Frames::from(rate) / 100).min(len / 2).max(1)
}

/// Sorts a lane's ranges and refuses ranges that are empty, overlap, or break the loop rules.
pub(crate) fn tidy_lane(lane: &mut EffectLane, loop_end: Option<Frames>) -> Result<()> {
    lane.ranges.sort_by_key(|r| r.start);
    if lane.ranges.iter().any(|r| r.end <= r.start) {
        return Err(Error::Invalid("an effect range must end after it starts".into()));
    }
    if let Some(l) = loop_end {
        if lane
            .ranges
            .iter()
            .any(|r| r.start < 0 || r.start >= l || r.end - r.start > l)
        {
            return Err(Error::Invalid(
                "with the loop on, an effect range must start before the loop end and be at most one loop long".into(),
            ));
        }
        if let (Some(first), Some(last)) = (lane.ranges.first(), lane.ranges.last())
            && lane.ranges.len() > 1
            && last.end - l > first.start
        {
            return Err(Error::Invalid(
                "effect ranges in one lane cannot overlap; use another lane".into(),
            ));
        }
    }
    if lane.ranges.windows(2).any(|w| matches!(w, [a, b] if a.end > b.start)) {
        return Err(Error::Invalid(
            "effect ranges in one lane cannot overlap; use another lane".into(),
        ));
    }
    Ok(())
}

/// Applies the lane to `out`, which holds timeline frames from `start`. Inside its ranges the processed sound
/// replaces the plain one, with short fades between them at the edges.
pub(crate) fn apply_lane(lane: &EffectLane, project: &Project, out: &mut [Vec<f32>; 2], start: Frames) {
    if !lane.enabled {
        return;
    }
    let loop_end = project.loop_end.filter(|l| *l > 0);
    let frames = out[0].len();
    // How much of the processed sound each frame gets, and the range it is in.
    let mut weights: Vec<(f32, usize)> = vec![(0.0, 0); frames];
    for (index, range) in lane.ranges.iter().enumerate() {
        // A range round the whole loop has no edges: its end runs straight into its start.
        let whole_loop = loop_end.is_some_and(|l| range.end - range.start >= l);
        let edge = edge_frames(project.sample_rate, range.end - range.start) as f32;
        for (i, w) in weights.iter_mut().enumerate() {
            let t = start.saturating_add(i as Frames);
            let Some(into) = position_in(range, t, loop_end) else {
                continue;
            };
            let left_in = (range.end - range.start - into) as f32;
            let weight = if whole_loop {
                1.0
            } else {
                ((into as f32 + 1.0) / edge).min(left_in / edge).min(1.0)
            };
            *w = (weight, index);
        }
    }
    if weights.iter().all(|(w, _)| *w == 0.0) {
        return;
    }
    let wet: Option<[Vec<f32>; 2]> = lane.effect.filter().map(|(hz, stages)| {
        let mut wet = out.clone();
        for channel in &mut wet {
            for &q in stages {
                Biquad::new(lane.effect, hz, q, project.sample_rate).run(channel);
            }
        }
        wet
    });
    for (c, channel) in out.iter_mut().enumerate() {
        for (i, (sample, &(weight, range))) in channel.iter_mut().zip(&weights).enumerate() {
            if weight == 0.0 {
                continue;
            }
            let processed = match &wet {
                Some(wet) => wet.get(c).and_then(|w| w.get(i)).copied().unwrap_or(*sample),
                None => *sample * lane.ranges.get(range).map_or(1.0, |r| r.gain),
            };
            *sample += weight * (processed - *sample);
        }
    }
}

/// Keeps the cutoff below the highest frequency the rate can hold.
fn cutoff(hz: f32, rate: u32) -> f32 {
    hz.min(rate as f32 * 0.45).max(1.0)
}

/// One second-order filter stage, from the formulas of the "Audio EQ Cookbook".
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    fn new(effect: Effect, hz: f32, q: f64, rate: u32) -> Biquad {
        let w0 = std::f64::consts::TAU * f64::from(cutoff(hz, rate)) / f64::from(rate.max(1));
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = match effect {
            Effect::HighPass { .. } => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            _ => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
        };
        Biquad {
            b: b.map(|v| v / a0),
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
        }
    }

    /// Filters in place, starting from silence.
    fn run(&self, samples: &mut [f32]) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for s in samples {
            let x = f64::from(*s);
            let y = self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            *s = y as f32;
        }
    }
}

/// How far into the range timeline frame `t` is, if it is in it. With a loop, a range past the loop end
/// also covers the start of the next loop.
fn position_in(range: &EffectRange, t: Frames, loop_end: Option<Frames>) -> Option<Frames> {
    let t = loop_end.map_or(t, |l| t.rem_euclid(l));
    let shifts = [0, loop_end.unwrap_or(0)];
    shifts
        .iter()
        .take(if loop_end.is_some() { 2 } else { 1 })
        .map(|s| t + s)
        .find(|&t| t >= range.start && t < range.end)
        .map(|t| t - range.start)
}

/// Measures what the effects need from the audio under them, lane by lane from the top, so each lane
/// measures what the lanes above it produce.
pub(crate) fn measure(project: &mut Project) {
    for ti in 0..project.tracks.len() {
        let lanes = project.tracks.get(ti).map_or(0, |t| t.effects.len());
        for li in 0..lanes {
            let gains: Vec<f32> = {
                let Some(track) = project.tracks.get(ti) else { continue };
                let Some(lane) = track.effects.get(li) else { continue };
                lane.ranges
                    .iter()
                    .map(|r| normalize_gain(&lane.effect, || render_track(project, track, r.start, span(r), li)))
                    .collect()
            };
            if let Some(lane) = project.tracks.get_mut(ti).and_then(|t| t.effects.get_mut(li)) {
                lane.ranges.iter_mut().zip(gains).for_each(|(r, g)| r.gain = g);
            }
        }
    }
    for li in 0..project.mix_effects.len() {
        let gains: Vec<f32> = {
            let Some(lane) = project.mix_effects.get(li) else {
                continue;
            };
            lane.ranges
                .iter()
                .map(|r| normalize_gain(&lane.effect, || mix_upto(project, r.start, span(r), li)))
                .collect()
        };
        if let Some(lane) = project.mix_effects.get_mut(li) {
            lane.ranges.iter_mut().zip(gains).for_each(|(r, g)| r.gain = g);
        }
    }
}

fn span(range: &EffectRange) -> usize {
    usize::try_from(range.end - range.start).unwrap_or(0)
}

/// The gain that brings the loudest sample to the target; silence is left alone. Other effects measure nothing.
fn normalize_gain(effect: &Effect, audio: impl FnOnce() -> [Vec<f32>; 2]) -> f32 {
    let Effect::Normalize { peak_db } = *effect else {
        return 1.0;
    };
    let peak = audio().iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak <= 1e-6 {
        return 1.0;
    }
    (db_to_amp(peak_db) / peak).clamp(db_to_amp(-80.0), db_to_amp(40.0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Command, Session, Source, TrackId, render};

    const RATE: u32 = 1000;

    /// A constant signal, so the peak is easy to check everywhere.
    fn level(value: f32, frames: usize) -> Arc<Source> {
        Arc::new(Source {
            path: None,
            name: "level".into(),
            sample_rate: RATE,
            channels: vec![vec![value; frames]],
        })
    }

    fn session(clips: &[(f32, Frames)]) -> (Session, TrackId) {
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        for &(value, start) in clips {
            s.execute(Command::AddClip {
                track,
                source: level(value, 1000),
                start,
            })
            .unwrap();
        }
        (s, track)
    }

    fn add_lane(s: &mut Session, track: Option<TrackId>, peak_db: f32, ranges: &[(Frames, Frames)]) -> LaneId {
        s.execute(Command::AddEffectLane {
            track,
            effect: Effect::Normalize { peak_db },
        })
        .unwrap();
        let p = s.project();
        let lane = match track {
            Some(t) => p.track(t).unwrap().effects.last().unwrap().id,
            None => p.mix_effects.last().unwrap().id,
        };
        for &(start, end) in ranges {
            s.execute(Command::AddEffectRange { lane, start, end }).unwrap();
        }
        lane
    }

    #[test]
    fn normalize_across_two_clips_from_inside_one_to_inside_the_other() {
        // 0.25 from 0 to 1000, 0.5 from 1000 to 2000.
        let (mut s, track) = session(&[(0.25, 0), (0.5, 1000)]);
        add_lane(&mut s, Some(track), -6.0, &[(500, 1500)]);
        let [left, _] = render(s.project(), 0, 2000);
        let target = db_to_amp(-6.0);
        assert!((left[400] - 0.25).abs() < 1e-6, "before the range: untouched");
        assert!((left[1200] - target).abs() < 1e-4, "{}", left[1200]);
        assert!(
            (left[700] - 0.25 * target / 0.5).abs() < 1e-4,
            "same gain for both clips: {}",
            left[700]
        );
        assert!((left[1700] - 0.5).abs() < 1e-6, "after the range: untouched");
        assert!(left.iter().all(|v| *v <= target + 1e-4), "nothing goes past the peak");
    }

    #[test]
    fn effects_come_before_the_track_volume_and_the_mix_lanes_after_all_tracks() {
        let (mut s, track) = session(&[(0.25, 0)]);
        add_lane(&mut s, Some(track), -1.0, &[(0, 1000)]);
        s.execute(Command::SetTrackGain { track, db: -6.0 }).unwrap();
        let [left, _] = render(s.project(), 0, 1000);
        assert!((left[500] - db_to_amp(-7.0)).abs() < 1e-4, "{}", left[500]);

        s.execute(Command::AddTrack { name: "u".into() }).unwrap();
        let other = s.project().tracks[1].id;
        s.execute(Command::AddClip {
            track: other,
            source: level(0.1, 1000),
            start: 0,
        })
        .unwrap();
        add_lane(&mut s, None, -3.0, &[(0, 1000)]);
        let [left, _] = render(s.project(), 0, 1000);
        assert!((left[500] - db_to_amp(-3.0)).abs() < 1e-4, "{}", left[500]);
    }

    #[test]
    fn a_range_past_the_loop_end_also_covers_the_start_and_a_whole_loop_has_no_seam() {
        let (mut s, track) = session(&[(0.25, 0)]);
        s.execute(Command::SetLoopEnd { at: Some(1000) }).unwrap();
        let lane = add_lane(&mut s, Some(track), -6.0, &[(800, 1200)]);
        let [left, _] = render(s.project(), 0, 1000);
        let loud = 0.25 * db_to_amp(-6.0) / 0.25;
        assert!((left[100] - loud).abs() < 1e-4, "{}", left[100]);
        assert!((left[500] - 0.25).abs() < 1e-6);

        let range = s.project().lane(lane).unwrap().ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 1000,
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 1000);
        assert!(left.iter().all(|v| (v - loud).abs() < 1e-4), "no dip at the seam");
    }

    #[test]
    fn ranges_in_one_lane_cannot_overlap() {
        let (mut s, track) = session(&[(0.25, 0)]);
        let lane = add_lane(&mut s, Some(track), -6.0, &[(0, 500)]);
        assert!(
            s.execute(Command::AddEffectRange {
                lane,
                start: 400,
                end: 900
            })
            .is_err()
        );
        assert!(
            s.execute(Command::AddEffectRange {
                lane,
                start: 500,
                end: 900
            })
            .is_ok()
        );
    }

    fn tone_session(hz: f32, effect: Effect) -> Session {
        let rate = 48_000;
        let samples = (0..rate)
            .map(|i| 0.5 * (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin())
            .collect();
        let source = Arc::new(Source {
            path: None,
            name: "tone".into(),
            sample_rate: rate,
            channels: vec![samples],
        });
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source,
            start: 0,
        })
        .unwrap();
        s.execute(Command::AddEffectLane {
            track: Some(track),
            effect,
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[0].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 0,
            end: 48_000,
        })
        .unwrap();
        s
    }

    /// The loudest sample in the middle of the second, after the filter settled.
    fn level_after(s: &Session) -> f32 {
        render(s.project(), 12_000, 24_000)[0]
            .iter()
            .fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn high_pass_removes_hum_and_low_pass_removes_hiss() {
        let high = Effect::HighPass {
            hz: 1000.0,
            steep: false,
        };
        let low = Effect::LowPass {
            hz: 1000.0,
            steep: false,
        };
        assert!(level_after(&tone_session(50.0, high)) < 0.01);
        // 6 kHz: at 48 kHz its samples hit the peak of the wave.
        assert!(level_after(&tone_session(6000.0, high)) > 0.48);
        assert!(level_after(&tone_session(50.0, low)) > 0.48);
        assert!(level_after(&tone_session(6000.0, low)) < 0.02);

        let gentle = level_after(&tone_session(4000.0, low));
        let steep = level_after(&tone_session(
            4000.0,
            Effect::LowPass {
                hz: 1000.0,
                steep: true,
            },
        ));
        assert!(steep < gentle / 8.0, "{steep} {gentle}");
    }

    /// Playback renders in pieces; with the warm-up they join like one long render, also over the loop seam.
    #[test]
    fn rendering_in_pieces_matches_one_render() {
        let mut s = tone_session(3000.0, Effect::LowPass { hz: 500.0, steep: true });
        let whole = render(s.project(), 0, 48_000);
        let mut pieces = vec![];
        for start in (0..48_000).step_by(512) {
            pieces.extend(render(s.project(), start, 512.min(48_000 - start as usize))[0].clone());
        }
        let most = whole[0]
            .iter()
            .zip(&pieces)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(most < 1e-3, "{most}");

        let range = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 30_000,
        })
        .unwrap();
        s.execute(Command::SetLoopEnd { at: Some(30_000) }).unwrap();
        let across = render(s.project(), 29_744, 512);
        let mut joined = render(s.project(), 29_744, 256)[0].clone();
        joined.extend(render(s.project(), 0, 256)[0].clone());
        let most = across[0]
            .iter()
            .zip(&joined)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(most < 1e-3, "{most}");
    }
}
