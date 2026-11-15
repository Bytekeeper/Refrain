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
/// With a loop end, the track is a circle: the last clip comes before the first, one loop earlier.
pub fn edge_fades(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> (Fade, Fade) {
    let Some(clip) = clips.get(i) else {
        return (Fade::default(), Fade::default());
    };
    let before = overlap_before(clips, i, loop_end);
    let after = overlap_after(clips, i, loop_end);
    let fade_in = if before > 0 {
        Fade {
            len: before,
            shape: clip.crossfade_shape,
        }
    } else {
        clip.fade_in
    };
    let fade_out = match next_index(clips, i, loop_end).and_then(|n| clips.get(n)) {
        Some(next) if after > 0 => Fade {
            len: after,
            shape: next.crossfade_shape,
        },
        _ => clip.fade_out,
    };
    (fade_in, fade_out)
}

/// Start and end of the clip before `i`; before the first clip of a loop, the last one a loop earlier.
pub(crate) fn previous_span(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> Option<(Frames, Frames)> {
    match i.checked_sub(1) {
        Some(j) => clips.get(j).map(|c| (c.start, c.end())),
        None => {
            let l = loop_end?;
            let last = clips.last()?;
            Some((last.start.saturating_sub(l), last.end().saturating_sub(l)))
        }
    }
}

fn next_index(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> Option<usize> {
    if i + 1 < clips.len() {
        Some(i + 1)
    } else if loop_end.is_some() && i < clips.len() {
        Some(0)
    } else {
        None
    }
}

pub fn overlap_before(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> Frames {
    match (previous_span(clips, i, loop_end), clips.get(i)) {
        (Some((_, end)), Some(clip)) => end.saturating_sub(clip.start).max(0),
        _ => 0,
    }
}

pub fn overlap_after(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> Frames {
    next_index(clips, i, loop_end).map_or(0, |n| overlap_before(clips, n, loop_end))
}

/// Mixes `frames` stereo frames from timeline position `start`. With a loop end, positions repeat
/// every loop and audio past the loop end plays again from 0.
pub fn render(project: &Project, start: Frames, frames: usize) -> [Vec<f32>; 2] {
    let mut out = [vec![0.0; frames], vec![0.0; frames]];
    let Some(l) = project.loop_end.filter(|l| *l > 0) else {
        mix_into(project, &mut out, 0, start, frames);
        return out;
    };
    let mut done = 0;
    while done < frames {
        let at = start.saturating_add(done as Frames).rem_euclid(l);
        let n = usize::try_from(l - at).unwrap_or(usize::MAX).min(frames - done);
        mix_into(project, &mut out, done, at, n);
        done += n;
    }
    out
}

/// Adds timeline frames `from..from + n` to `out` from index `at`. With a loop end, this range must lie in one loop.
fn mix_into(project: &Project, out: &mut [Vec<f32>; 2], at: usize, from: Frames, n: usize) {
    let to = from.saturating_add(Frames::try_from(n).unwrap_or(Frames::MAX));
    let [left, right] = out;
    for track in project.tracks.iter().filter(|t| !t.muted) {
        let track_amp = db_to_amp(track.gain_db);
        for (index, clip) in track.clips.iter().enumerate() {
            let fades = edge_fades(&track.clips, index, project.loop_end);
            let stereo = clip.source.channels.len() >= 2;
            for shift in loop_shifts(clip, from, to, project.loop_end) {
                let clip_start = clip.start.saturating_sub(shift);
                for pos in clip_start.max(from)..clip_start.saturating_add(clip.len).min(to) {
                    let Ok(i) = usize::try_from(pos - from) else { continue };
                    let rel = pos - clip_start;
                    let src = clip.offset.saturating_add(rel);
                    let amp = track_amp * clip_amp(clip, fades, rel);
                    let l = clip.source.sample(0, src);
                    let r = if stereo { clip.source.sample(1, src) } else { l };
                    if let (Some(ol), Some(or)) = (left.get_mut(at + i), right.get_mut(at + i)) {
                        *ol += l * amp;
                        *or += r * amp;
                    }
                }
            }
        }
    }
}

/// How far back the clip is moved to reach `from..to`: 0, and with a loop each whole number of loops
/// that brings a part of it into the range. The overlap rules keep a clip shorter than two loops.
fn loop_shifts(clip: &Clip, from: Frames, to: Frames, loop_end: Option<Frames>) -> Vec<Frames> {
    let Some(l) = loop_end.filter(|l| *l > 0) else {
        return vec![0];
    };
    let first = clip.start.saturating_sub(to).div_euclid(l).saturating_add(1);
    let last = clip.end().saturating_sub(from).saturating_sub(1).div_euclid(l);
    (first..=last.min(first.saturating_add(3)))
        .map(|k| k.saturating_mul(l))
        .collect()
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
            loop_end: None,
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
        assert_eq!(edge_fades(&clips, 0, None).0.len, 10);
        assert_eq!(edge_fades(&clips, 0, None).1.len, 20);
        assert_eq!(edge_fades(&clips, 1, None).0.len, 20);
    }

    #[test]
    fn volume_curve_is_linear_in_db_and_holds_at_ends() {
        let points = [GainPoint { at: 100, db: 0.0 }, GainPoint { at: 200, db: -20.0 }];
        assert_eq!(envelope_db(&points, 0), 0.0);
        assert!((envelope_db(&points, 150) + 10.0).abs() < 1e-6);
        assert_eq!(envelope_db(&points, 500), -20.0);
    }
}
