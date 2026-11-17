//! Mixing: clips with their fades, curves and crossfades, track and mix effects, and the loop.

use std::f32::consts::FRAC_PI_2;

use crate::effects::{Memory, apply_lane, lane_into};
use crate::{Clip, Fade, FadeShape, Frames, GainPoint, MIN_DB, Project, Track};
use crate::{EffectLane, EffectRange};

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
    mix_upto(project, start, frames, project.mix_effects.len())
}

/// The whole project as heard (with the loop on: one loop): each track's clips, then its effect lanes from the
/// top, at its volume; then the mix lanes. Every effect hears all of its input, before and after.
pub fn mixdown(project: &Project) -> [Vec<f32>; 2] {
    let len = usize::try_from(project.len()).unwrap_or(0);
    let mut mix = [vec![0.0; len], vec![0.0; len]];
    let mut sound = [vec![0.0; len], vec![0.0; len]];
    for track in project.tracks.iter().filter(|t| !t.muted) {
        sound.iter_mut().for_each(|c| c.fill(0.0));
        clips_into(project, track, &mut sound, 0, 0, len);
        for lane in &track.effects {
            lane_into(lane, project, &mut sound);
        }
        let amp = db_to_amp(track.gain_db);
        for (mix, sound) in mix.iter_mut().zip(&sound) {
            mix.iter_mut().zip(sound).for_each(|(m, s)| *m += s * amp);
        }
    }
    for lane in &project.mix_effects {
        lane_into(lane, project, &mut mix);
    }
    mix
}

/// Renders the mix piece after piece for playback. The effects keep their memory of the sound between pieces,
/// so echo and reverb do not have to render seconds of the past again for each piece.
#[derive(Default)]
pub struct Renderer {
    memory: Memory,
    /// Where the last piece ended: a piece starting there goes on from the memory.
    next: Option<Frames>,
}

impl Renderer {
    pub fn render(&mut self, project: &Project, start: Frames, frames: usize) -> [Vec<f32>; 2] {
        let lanes = project.mix_effects.len();
        let warmup = if self.next == Some(start) {
            self.memory.forget_removed(project);
            0
        } else {
            self.memory = Memory::default();
            // After a jump, a full warm-up for a long reverb would stall the sound, so the tails before the jump
            // are left out, as on a mixing desk.
            mix_warmup(project, lanes, start, frames).min(Frames::from(project.sample_rate) / 4)
        };
        let out = mix_with(
            project,
            &mut self.memory,
            start - warmup,
            frames + warmup as usize,
            lanes,
        );
        let end = start.saturating_add(Frames::try_from(frames).unwrap_or(0));
        self.next = Some(project.loop_end.filter(|l| *l > 0).map_or(end, |l| end.rem_euclid(l)));
        drop_warmup(out, warmup as usize)
    }
}

/// The loudest sample of the mix, both sides, in each block of [`Levels::BLOCK`] frames of the whole project.
#[derive(Debug, Clone, PartialEq)]
pub struct Levels {
    pub peaks: Vec<f32>,
}

impl Levels {
    pub const BLOCK: usize = 256;

    /// The loudest sample in timeline frames `from..to`.
    pub fn peak(&self, from: Frames, to: Frames) -> f32 {
        let block = |f: Frames| usize::try_from(f.max(0)).unwrap_or(0) / Self::BLOCK;
        let blocks = block(from)..=block(to.saturating_sub(1)).min(self.peaks.len().saturating_sub(1));
        self.peaks
            .get(blocks)
            .map_or(0.0, |p| p.iter().fold(0.0, |m, v| m.max(*v)))
    }
}

/// [`Levels`] of the whole project (with the loop on: one loop). Renders piece by piece, keeping the effects'
/// memory between pieces as playback does; stops and returns `None` once `keep_going` says no.
pub fn levels(project: &Project, keep_going: impl Fn() -> bool) -> Option<Levels> {
    const PIECE: usize = Levels::BLOCK * 256;
    let len = usize::try_from(project.len()).unwrap_or(0);
    let lanes = project.mix_effects.len();
    let mut memory = Memory::default();
    let mut peaks = Vec::with_capacity(len.div_ceil(Levels::BLOCK));
    let mut start = 0;
    while start < len {
        if !keep_going() {
            return None;
        }
        let frames = PIECE.min(len - start);
        let at = Frames::try_from(start).unwrap_or(Frames::MAX);
        // The first piece hears the past it needs; later ones go on from the memory.
        let warmup = if start == 0 {
            mix_warmup(project, lanes, at, frames)
        } else {
            0
        };
        let out = mix_with(project, &mut memory, at - warmup, frames + warmup as usize, lanes);
        let [left, right] = drop_warmup(out, warmup as usize);
        for (l, r) in left.chunks(Levels::BLOCK).zip(right.chunks(Levels::BLOCK)) {
            peaks.push(l.iter().chain(r).fold(0.0f32, |m, s| m.max(s.abs())));
        }
        start += frames;
    }
    Some(Levels { peaks })
}

/// All tracks with their effects and volume, mixed, then the first `lanes` mix effects.
pub(crate) fn mix_upto(project: &Project, start: Frames, frames: usize, lanes: usize) -> [Vec<f32>; 2] {
    let warmup = mix_warmup(project, lanes, start, frames);
    let out = mix_with(
        project,
        &mut Memory::default(),
        start - warmup,
        frames + warmup as usize,
        lanes,
    );
    drop_warmup(out, warmup as usize)
}

fn mix_with(project: &Project, memory: &mut Memory, from: Frames, frames: usize, lanes: usize) -> [Vec<f32>; 2] {
    let mut out = [vec![0.0; frames], vec![0.0; frames]];
    for track in project.tracks.iter().filter(|t| !t.muted) {
        let amp = db_to_amp(track.gain_db);
        let [left, right] = track_with(project, track, memory, from, frames, track.effects.len());
        let [out_left, out_right] = &mut out;
        for (o, s) in out_left.iter_mut().zip(left).chain(out_right.iter_mut().zip(right)) {
            *o += s * amp;
        }
    }
    for lane in project.mix_effects.iter().take(lanes) {
        apply_lane(lane, project, &mut out, from, memory);
    }
    out
}

/// One track's clips and crossfades, then its first `lanes` effects. Without the track volume.
pub(crate) fn render_track(
    project: &Project,
    track: &Track,
    start: Frames,
    frames: usize,
    lanes: usize,
) -> [Vec<f32>; 2] {
    let effects = track.effects.get(..lanes).unwrap_or(&track.effects);
    let warmup = lanes_warmup(project, effects, start, frames);
    let out = track_with(
        project,
        track,
        &mut Memory::default(),
        start - warmup,
        frames + warmup as usize,
        lanes,
    );
    drop_warmup(out, warmup as usize)
}

fn track_with(
    project: &Project,
    track: &Track,
    memory: &mut Memory,
    from: Frames,
    frames: usize,
    lanes: usize,
) -> [Vec<f32>; 2] {
    let mut out = [vec![0.0; frames], vec![0.0; frames]];
    match project.loop_end.filter(|l| *l > 0) {
        None => clips_into(project, track, &mut out, 0, from, frames),
        Some(l) => {
            let mut done = 0;
            while done < frames {
                let at = from.saturating_add(done as Frames).rem_euclid(l);
                let n = usize::try_from(l - at).unwrap_or(usize::MAX).min(frames - done);
                clips_into(project, track, &mut out, done, at, n);
                done += n;
            }
        }
    }
    for lane in track.effects.iter().take(lanes) {
        apply_lane(lane, project, &mut out, from, memory);
    }
    out
}

/// How much earlier to start rendering so every effect that sounds in the piece has heard enough of the past.
/// Stacked lanes add up: a lane needs its input to be right already from its own warm-up on.
fn lanes_warmup(project: &Project, lanes: &[EffectLane], start: Frames, frames: usize) -> Frames {
    let end = start.saturating_add(Frames::try_from(frames).unwrap_or(Frames::MAX));
    let rate = project.sample_rate;
    // With a loop every range counts: what comes before 0 is the end of the loop.
    let sounds = |r: &EffectRange| {
        project.loop_end.is_some() || (r.start < end && r.end.saturating_add(r.effect.tail(rate)) > start)
    };
    lanes
        .iter()
        .filter(|l| l.enabled)
        .map(|l| {
            let ranges = l.ranges.iter().filter(|r| sounds(r));
            ranges.map(|r| r.effect.warmup(rate)).max().unwrap_or(0)
        })
        .fold(0, Frames::saturating_add)
}

fn mix_warmup(project: &Project, lanes: usize, start: Frames, frames: usize) -> Frames {
    let mix = project.mix_effects.get(..lanes).unwrap_or(&project.mix_effects);
    let after = lanes_warmup(project, mix, start, frames);
    let longer = frames.saturating_add(usize::try_from(after).unwrap_or(0));
    let tracks = project
        .tracks
        .iter()
        .map(|t| lanes_warmup(project, &t.effects, start - after, longer))
        .max()
        .unwrap_or(0);
    after.saturating_add(tracks)
}

fn drop_warmup([mut left, mut right]: [Vec<f32>; 2], warmup: usize) -> [Vec<f32>; 2] {
    left.drain(..warmup.min(left.len()));
    right.drain(..warmup.min(right.len()));
    [left, right]
}

/// Adds the track's timeline frames `from..from + n` to `out` from index `at`. With a loop end, this range must
/// lie in one loop.
fn clips_into(project: &Project, track: &Track, out: &mut [Vec<f32>; 2], at: usize, from: Frames, n: usize) {
    let to = from.saturating_add(Frames::try_from(n).unwrap_or(Frames::MAX));
    let [left, right] = out;
    for (index, clip) in track.clips.iter().enumerate() {
        let fades = edge_fades(&track.clips, index, project.loop_end);
        for shift in loop_shifts(clip, from, to, project.loop_end) {
            let clip_start = clip.start.saturating_sub(shift);
            for pos in clip_start.max(from)..clip_start.saturating_add(clip.len).min(to) {
                let Ok(i) = usize::try_from(pos - from) else { continue };
                let rel = pos - clip_start;
                let src = clip.offset.saturating_add(rel);
                let amp = clip_amp(clip, fades, rel);
                let (l, r) = clip.frame(src);
                if let (Some(ol), Some(or)) = (left.get_mut(at + i), right.get_mut(at + i)) {
                    *ol += l * amp;
                    *or += r * amp;
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

/// How well the waveforms of two clips match over timeline frames `from..to`: 1 is the same waveform,
/// 0 unrelated sound, -1 opposite waveforms that cancel out when mixed. Each moment counts as much as both
/// clips are heard there (fades and volume), so in a crossfade the middle matters most. `None` where either is silent.
pub fn similarity(a: (&Clip, (Fade, Fade)), b: (&Clip, (Fade, Fade)), from: Frames, to: Frames) -> Option<f32> {
    // Enough points for a stable number at screen speed.
    similarity_sampled(a, b, from, to, 200_000)
}

/// [`similarity`] from at most about `max_points` evenly spread frames.
pub(crate) fn similarity_sampled(
    a: (&Clip, (Fade, Fade)),
    b: (&Clip, (Fade, Fade)),
    from: Frames,
    to: Frames,
    max_points: Frames,
) -> Option<f32> {
    let step = (to.saturating_sub(from) / max_points.max(1)).max(1);
    let at = |clip: &Clip, channel: usize, pos: Frames| {
        let (left, right) = clip.frame(clip.offset.saturating_add(pos - clip.start));
        f64::from(if channel == 0 { left } else { right })
    };
    let heard = |(clip, fades): (&Clip, (Fade, Fade)), pos: Frames| f64::from(clip_amp(clip, fades, pos - clip.start));
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    let mut pos = from;
    while pos < to {
        // Weighting the moments, not the samples: fading each clip on its own would keep even identical waves below 1.
        let weight = heard(a, pos) * heard(b, pos);
        // Mono counts as the same sound on both sides, as it is played.
        for channel in 0..2 {
            let (x, y) = (at(a.0, channel, pos), at(b.0, channel, pos));
            ab += weight * x * y;
            aa += weight * x * x;
            bb += weight * y * y;
        }
        pos = pos.saturating_add(step);
    }
    let norm = (aa * bb).sqrt();
    (norm > 1e-12).then(|| (ab / norm) as f32)
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
    use crate::{ChannelMode, ClipId, Source, Track, TrackId};

    fn constant(value: f32, frames: usize) -> Arc<Source> {
        Arc::new(Source {
            path: None,
            name: "c".into(),
            sample_rate: 1000,
            channels: vec![vec![value; frames]],
            generated: None,
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
            channels: ChannelMode::Stereo,
        }
    }

    fn one_track(clips: Vec<Clip>) -> Project {
        let track = Track {
            id: TrackId(1),
            name: "t".into(),
            gain_db: 0.0,
            muted: false,
            clips,
            effects: vec![],
        };
        Project {
            sample_rate: 1000,
            tracks: vec![track],
            next_id: 10,
            loop_end: None,
            relocated: vec![],
            mix_effects: vec![],
            export: Default::default(),
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
    fn similarity_of_same_opposite_and_unrelated_sound() {
        let sine = |freq: f32, sign: f32| {
            let wave = (0..8000).map(|i| sign * (i as f32 * freq * 0.001 * std::f32::consts::TAU).sin());
            Arc::new(Source {
                path: None,
                name: "s".into(),
                sample_rate: 1000,
                channels: vec![wave.collect()],
                generated: None,
            })
        };
        let a = clip(1, sine(50.0, 1.0), 0, FadeShape::Linear);
        // The same audio, but starting 100 frames later in the file: one whole period of 50 Hz at 1000 Hz is 20 frames.
        let mut same = clip(2, sine(50.0, 1.0), 0, FadeShape::Linear);
        same.offset = 100;
        let opposite = clip(3, sine(50.0, -1.0), 0, FadeShape::Linear);
        let unrelated = clip(4, sine(73.0, 1.0), 0, FadeShape::Linear);
        let none = (Fade::default(), Fade::default());
        let alike = |b: &Clip| similarity((&a, none), (b, none), 0, 4000).unwrap();
        assert!((alike(&same) - 1.0).abs() < 1e-4, "{}", alike(&same));
        assert!((alike(&opposite) + 1.0).abs() < 1e-4);
        assert!(alike(&unrelated).abs() < 0.05, "{}", alike(&unrelated));
        let silent = clip(5, constant(0.0, 8000), 0, FadeShape::Linear);
        assert_eq!(similarity((&a, none), (&silent, none), 0, 4000), None);
        // A crossfade changes which moments count, not the result for the same waveform.
        let fade = |len| Fade {
            len,
            shape: FadeShape::Linear,
        };
        // Starting at 4000 and 100 frames into its file, `later` is 3900 frames behind `a`: still whole periods.
        let mut later = same.clone();
        later.start = 4000;
        let crossfade = similarity((&a, (fade(0), fade(4000))), (&later, (fade(4000), fade(0))), 4000, 8000).unwrap();
        assert!((crossfade - 1.0).abs() < 1e-4, "{crossfade}");
    }

    #[test]
    fn channel_modes_pick_or_mix_the_sides() {
        let source = Arc::new(Source {
            path: None,
            name: "lr".into(),
            sample_rate: 1000,
            channels: vec![vec![1.0; 10], vec![0.5; 10]],
            generated: None,
        });
        for (mode, expected) in [
            (ChannelMode::Stereo, (1.0, 0.5)),
            (ChannelMode::Mono, (0.75, 0.75)),
            (ChannelMode::Left, (1.0, 1.0)),
            (ChannelMode::Right, (0.5, 0.5)),
        ] {
            let mut c = clip(1, source.clone(), 0, FadeShape::Linear);
            c.channels = mode;
            let [l, r] = render(&one_track(vec![c]), 0, 10);
            assert_eq!((l[5], r[5]), expected, "{mode:?}");
        }
    }

    #[test]
    fn volume_curve_is_linear_in_db_and_holds_at_ends() {
        let points = [GainPoint { at: 100, db: 0.0 }, GainPoint { at: 200, db: -20.0 }];
        assert_eq!(envelope_db(&points, 0), 0.0);
        assert!((envelope_db(&points, 150) + 10.0).abs() < 1e-6);
        assert_eq!(envelope_db(&points, 500), -20.0);
    }
}
