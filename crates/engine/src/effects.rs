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
}

impl Effect {
    /// One of each kind with its usual settings, in menu order.
    pub fn defaults() -> Vec<Effect> {
        vec![Effect::Normalize { peak_db: -1.0 }]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Effect::Normalize { .. } => "Normalize",
        }
    }

    /// How long before a range the effect already changes the sound.
    pub fn lead_in(&self, _rate: u32) -> Frames {
        match self {
            Effect::Normalize { .. } => 0,
        }
    }

    /// How long after a range the effect still sounds.
    pub fn tail(&self, _rate: u32) -> Frames {
        match self {
            Effect::Normalize { .. } => 0,
        }
    }

    fn check(self) -> Result<Effect> {
        match self {
            Effect::Normalize { peak_db } if peak_db.is_finite() && (-60.0..=0.0).contains(&peak_db) => Ok(self),
            Effect::Normalize { .. } => Err(Error::Invalid("normalize goes to a peak between -60 and 0 dB".into())),
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

/// Applies the lane to `out`, which holds timeline frames from `start`.
pub(crate) fn apply_lane(lane: &EffectLane, project: &Project, out: &mut [Vec<f32>; 2], start: Frames) {
    if !lane.enabled {
        return;
    }
    let loop_end = project.loop_end.filter(|l| *l > 0);
    let [left, right] = out;
    for range in &lane.ranges {
        // A range round the whole loop has no edges: its end runs straight into its start.
        let whole_loop = loop_end.is_some_and(|l| range.end - range.start >= l);
        let edge = if whole_loop {
            0.0
        } else {
            edge_frames(project.sample_rate, range.end - range.start) as f32
        };
        match lane.effect {
            Effect::Normalize { .. } => {
                for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
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
                    let amp = 1.0 + weight * (range.gain - 1.0);
                    *l *= amp;
                    *r *= amp;
                }
            }
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

/// The gain that brings the loudest sample to the target; silence is left alone.
fn normalize_gain(effect: &Effect, audio: impl FnOnce() -> [Vec<f32>; 2]) -> f32 {
    let Effect::Normalize { peak_db } = *effect;
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
}
