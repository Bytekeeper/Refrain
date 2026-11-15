use std::f32::consts::FRAC_PI_2;

use crate::{Clip, Fade, FadeShape, Frames, GainPoint, MIN_DB, Project};

/// Gain of a fade-in at progress `t` (0 = start, 1 = done).
pub fn fade_gain(shape: FadeShape, t: f32) -> f32 {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    match shape {
        FadeShape::Linear => t,
        FadeShape::EqualPower => (t * FRAC_PI_2).sin(),
    }
}

/// The fade-in and fade-out clip `i` of a track really uses: where a neighbour overlaps, the
/// overlap is the fade, so both sides of a crossfade always cover the same frames.
pub fn edge_fades(clips: &[Clip], i: usize) -> (Fade, Fade) {
    let Some(clip) = clips.get(i) else {
        return (Fade::default(), Fade::default());
    };
    let before = overlap_before(clips, i);
    let after = overlap_after(clips, i);
    let fade_in = if before > 0 {
        Fade {
            len: before,
            shape: clip.crossfade_shape,
        }
    } else {
        clip.fade_in
    };
    let fade_out = match clips.get(i + 1) {
        Some(next) if after > 0 => Fade {
            len: after,
            shape: next.crossfade_shape,
        },
        _ => clip.fade_out,
    };
    (fade_in, fade_out)
}

pub(crate) fn overlap_before(clips: &[Clip], i: usize) -> Frames {
    match (i.checked_sub(1).and_then(|j| clips.get(j)), clips.get(i)) {
        (Some(prev), Some(clip)) => prev.end().saturating_sub(clip.start).max(0),
        _ => 0,
    }
}

pub(crate) fn overlap_after(clips: &[Clip], i: usize) -> Frames {
    overlap_before(clips, i + 1)
}

/// Mixes `frames` stereo frames from timeline position `start`.
pub fn render(project: &Project, start: Frames, frames: usize) -> [Vec<f32>; 2] {
    let mut left = vec![0.0; frames];
    let mut right = vec![0.0; frames];
    let end = start.saturating_add(Frames::try_from(frames).unwrap_or(Frames::MAX));
    for track in project.tracks.iter().filter(|t| !t.muted) {
        let track_amp = db_to_amp(track.gain_db);
        for (index, clip) in track.clips.iter().enumerate() {
            let fades = edge_fades(&track.clips, index);
            let stereo = clip.source.channels.len() >= 2;
            for pos in clip.start.max(start)..clip.end().min(end) {
                let Ok(i) = usize::try_from(pos - start) else { continue };
                let rel = pos - clip.start;
                let src = clip.offset.saturating_add(rel);
                let amp = track_amp * clip_amp(clip, fades, rel);
                let l = clip.source.sample(0, src);
                let r = if stereo { clip.source.sample(1, src) } else { l };
                if let (Some(ol), Some(or)) = (left.get_mut(i), right.get_mut(i)) {
                    *ol += l * amp;
                    *or += r * amp;
                }
            }
        }
    }
    [left, right]
}

/// Linear gain at `rel` frames into the clip: volume curve times the fades from [`edge_fades`].
pub fn clip_amp(clip: &Clip, (fade_in, fade_out): (Fade, Fade), rel: Frames) -> f32 {
    let mut amp = db_to_amp(envelope_db(&clip.gain, rel));
    if rel < fade_in.len {
        amp *= fade_gain(fade_in.shape, ratio(rel, fade_in.len));
    }
    // Counted so that a fade-out and a fade-in over the same frames add up to exactly 1.
    let from_end = clip.len - rel;
    if from_end <= fade_out.len {
        amp *= fade_gain(fade_out.shape, ratio(from_end, fade_out.len));
    }
    amp
}

/// Straight lines in dB between points; the first and last values hold beyond the ends.
pub(crate) fn envelope_db(points: &[GainPoint], at: Frames) -> f32 {
    let Some(first) = points.first() else { return 0.0 };
    if at <= first.at {
        return first.db;
    }
    for pair in points.windows(2) {
        if let [a, b] = pair
            && at <= b.at
        {
            return a.db + (b.db - a.db) * ratio(at - a.at, b.at - a.at);
        }
    }
    points.last().map_or(0.0, |p| p.db)
}

pub(crate) fn db_to_amp(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

fn ratio(num: Frames, den: Frames) -> f32 {
    if den <= 0 { 1.0 } else { num as f32 / den as f32 }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{ClipId, Source, Track, TrackId};

    fn constant(value: f32, frames: usize) -> Arc<Source> {
        Arc::new(Source {
            path: None,
            name: "c".into(),
            sample_rate: 1000,
            channels: vec![vec![value; frames]],
        })
    }

    fn clip(id: u64, source: Arc<Source>, start: Frames, shape: FadeShape) -> Clip {
        Clip {
            id: ClipId(id),
            len: source.frames(),
            source,
            start,
            offset: 0,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            crossfade_shape: shape,
            gain: vec![],
        }
    }

    fn one_track(clips: Vec<Clip>) -> Project {
        let track = Track {
            id: TrackId(1),
            name: "t".into(),
            gain_db: 0.0,
            muted: false,
            clips,
        };
        Project {
            sample_rate: 1000,
            tracks: vec![track],
            next_id: 10,
        }
    }

    #[test]
    fn linear_crossfade_keeps_constant_level() {
        let a = clip(1, constant(1.0, 100), 0, FadeShape::Linear);
        let b = clip(2, constant(1.0, 100), 50, FadeShape::Linear);
        let [l, r] = render(&one_track(vec![a, b]), 0, 150);
        for (i, (l, r)) in l.iter().zip(&r).enumerate() {
            assert!((l - 1.0).abs() < 1e-6, "frame {i}: {l}");
            assert_eq!(l, r);
        }
    }

    #[test]
    fn equal_power_crossfade_keeps_constant_power() {
        // Silence in one clip isolates the other clip's gain.
        let gain_a = render(
            &one_track(vec![
                clip(1, constant(1.0, 128), 0, FadeShape::Linear),
                clip(2, constant(0.0, 128), 64, FadeShape::EqualPower),
            ]),
            64,
            64,
        );
        let gain_b = render(
            &one_track(vec![
                clip(1, constant(0.0, 128), 0, FadeShape::Linear),
                clip(2, constant(1.0, 128), 64, FadeShape::EqualPower),
            ]),
            64,
            64,
        );
        for (i, (a, b)) in gain_a[0].iter().zip(&gain_b[0]).enumerate() {
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "frame {i}: {a} {b}");
        }
    }

    #[test]
    fn free_fades_apply_only_where_nothing_overlaps() {
        let mut a = clip(1, constant(1.0, 100), 0, FadeShape::Linear);
        a.fade_out = Fade {
            len: 90,
            shape: FadeShape::Linear,
        };
        a.fade_in = Fade {
            len: 10,
            shape: FadeShape::Linear,
        };
        let b = clip(2, constant(1.0, 100), 80, FadeShape::Linear);
        let clips = vec![a, b];
        assert_eq!(edge_fades(&clips, 0).0.len, 10);
        assert_eq!(edge_fades(&clips, 0).1.len, 20);
        assert_eq!(edge_fades(&clips, 1).0.len, 20);
    }

    #[test]
    fn volume_curve_is_linear_in_db_and_holds_at_ends() {
        let points = [GainPoint { at: 100, db: 0.0 }, GainPoint { at: 200, db: -20.0 }];
        assert_eq!(envelope_db(&points, 0), 0.0);
        assert!((envelope_db(&points, 150) + 10.0).abs() < 1e-6);
        assert_eq!(envelope_db(&points, 500), -20.0);
    }
}
