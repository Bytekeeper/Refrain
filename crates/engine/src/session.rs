use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::effects::{measure, tidy_lane};
use crate::io::{load_source, resample};
use crate::mix::{edge_fades, envelope_db, overlap_after, overlap_before, similarity_sampled};
use crate::{
    ChannelMode, Clip, ClipId, Effect, EffectLane, EffectRange, Error, Fade, FadeShape, Frames, GainPoint, LaneId,
    MAX_DB, MIN_DB, Project, RangeId, Result, Source, Track, TrackId,
};

const MAX_UNDO: usize = 500;

/// Every change to the project. The window, and later a CLI or an agent, only edit through these.
#[derive(Debug, Clone)]
pub enum Command {
    AddTrack {
        name: String,
    },
    RemoveTrack {
        track: TrackId,
    },
    SetTrackGain {
        track: TrackId,
        db: f32,
    },
    SetTrackMute {
        track: TrackId,
        muted: bool,
    },
    AddClip {
        track: TrackId,
        source: Arc<Source>,
        start: Frames,
    },
    /// Moves a clip to `start` on `track` (which may be another track). Overlapping a neighbour
    /// makes a crossfade.
    MoveClip {
        clip: ClipId,
        track: TrackId,
        start: Frames,
    },
    /// Moves a clip to `start` on a new track below all others, named after the clip's audio file.
    MoveClipToNewTrack {
        clip: ClipId,
        start: Frames,
    },
    /// Sets the clip's timeline bounds. The audio stays where it was; only the visible part changes.
    TrimClip {
        clip: ClipId,
        start: Frames,
        end: Frames,
    },
    SplitClip {
        clip: ClipId,
        at: Frames,
    },
    /// A new lane at the bottom of a track's lanes, or of the mix's when `track` is `None`.
    AddEffectLane {
        track: Option<TrackId>,
        effect: Effect,
    },
    /// New settings for one range. Ranges drawn later in the lane start with them too.
    SetEffect {
        range: RangeId,
        effect: Effect,
    },
    /// The same settings for the whole lane: all its ranges and those drawn later.
    SetLaneEffect {
        lane: LaneId,
        effect: Effect,
    },
    /// Off lanes stay in place but do nothing, to compare with and without.
    SetEffectEnabled {
        lane: LaneId,
        enabled: bool,
    },
    /// One place up (applied earlier) or down (later).
    MoveEffectLane {
        lane: LaneId,
        down: bool,
    },
    RemoveEffectLane {
        lane: LaneId,
    },
    AddEffectRange {
        lane: LaneId,
        start: Frames,
        end: Frames,
    },
    /// Moves or resizes a range.
    SetEffectRange {
        range: RangeId,
        start: Frames,
        end: Frames,
    },
    RemoveEffectRange {
        range: RangeId,
    },
    /// A copy with the same trim, fades, volume points and channels, at `start` on `track`, or on a new track
    /// when `track` is `None`.
    DuplicateClip {
        clip: ClipId,
        track: Option<TrackId>,
        start: Frames,
    },
    DeleteClip {
        clip: ClipId,
    },
    /// Fades at an end that no other clip overlaps.
    SetFadeIn {
        clip: ClipId,
        fade: Fade,
    },
    SetFadeOut {
        clip: ClipId,
        fade: Fade,
    },
    /// Curve of the crossfade at the start of `clip`.
    SetCrossfadeShape {
        clip: ClipId,
        shape: FadeShape,
    },
    SetGainCurve {
        clip: ClipId,
        points: Vec<GainPoint>,
    },
    /// `None` ends looping. The loop end must be after 0.
    SetLoopEnd {
        at: Option<Frames>,
    },
    /// Shifts the crossfade into the start of `clip` by up to `range` frames to where the two waveforms
    /// match best, and picks the curve that suits that match. At the loop seam the loop end moves.
    MatchCrossfade {
        clip: ClipId,
        range: Frames,
    },
    /// Which channels the clips play; several clips change in one undo step.
    SetChannels {
        clips: Vec<ClipId>,
        mode: ChannelMode,
    },
    /// Converts all audio to `rate` and moves every position along, so everything stays at the same
    /// time. Audio from a file is converted again from the file, so changing the rate twice loses nothing.
    SetSampleRate {
        rate: u32,
    },
}

/// The open project with its undo history. Snapshots share sample data, so they are cheap.
#[derive(Debug, Default)]
pub struct Session {
    project: Arc<Project>,
    undo: Vec<Arc<Project>>,
    redo: Vec<Arc<Project>>,
}

impl Session {
    /// A session for a project read from a file. Refuses tracks that break the overlap rules.
    pub fn open(mut project: Project) -> Result<Session> {
        if project.loop_end.is_some_and(|l| l <= 0) {
            return Err(Error::Invalid("the loop end must be after 0".into()));
        }
        settle(&mut project)?;
        Ok(Session {
            project: Arc::new(project),
            undo: vec![],
            redo: vec![],
        })
    }

    pub fn project(&self) -> &Arc<Project> {
        &self.project
    }

    /// Runs the command on a copy; the project only changes when it succeeds.
    pub fn execute(&mut self, command: Command) -> Result<()> {
        let mut next = Project::clone(&self.project);
        apply(&mut next, command)?;
        settle(&mut next)?;
        let previous = std::mem::replace(&mut self.project, Arc::new(next));
        self.undo.push(previous);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        Ok(())
    }

    pub fn undo(&mut self) -> Result<()> {
        let previous = self.undo.pop().ok_or(Error::NothingToUndo)?;
        self.redo.push(std::mem::replace(&mut self.project, previous));
        Ok(())
    }

    pub fn redo(&mut self) -> Result<()> {
        let next = self.redo.pop().ok_or(Error::NothingToRedo)?;
        self.undo.push(std::mem::replace(&mut self.project, next));
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Whether the project or any step of its undo history reads audio from `path`.
    pub fn uses_file(&self, path: &Path) -> bool {
        let mut snapshots = std::iter::once(&self.project).chain(&self.undo).chain(&self.redo);
        snapshots.any(|p| {
            p.sources()
                .iter()
                .any(|s| p.file_of(s).is_some_and(|f| same_file(f, path)))
        })
    }

    /// The audio stored in `from` is now stored in `to`, in the project and the whole undo history. Not an undo
    /// step: it records where files are, not an edit.
    pub fn relocate(&mut self, from: &Path, to: &Path) {
        let snapshots = std::iter::once(&mut self.project)
            .chain(&mut self.undo)
            .chain(&mut self.redo);
        for snapshot in snapshots {
            let moved: Vec<Arc<Source>> = snapshot
                .sources()
                .into_iter()
                .filter(|s| snapshot.file_of(s).is_some_and(|f| same_file(f, from)))
                .cloned()
                .collect();
            if moved.is_empty() {
                continue;
            }
            let mut next = Project::clone(snapshot);
            next.relocated.retain(|(s, _)| !moved.iter().any(|m| Arc::ptr_eq(s, m)));
            next.relocated.extend(moved.into_iter().map(|s| (s, to.to_path_buf())));
            *snapshot = Arc::new(next);
        }
    }
}

/// The same path, or two spellings of one existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || std::fs::canonicalize(a).is_ok_and(|a| std::fs::canonicalize(b).is_ok_and(|b| a == b))
}

/// Checks the rules every project keeps after a change, then measures what the effects need from the audio.
fn settle(p: &mut Project) -> Result<()> {
    for track in &mut p.tracks {
        tidy(track, p.loop_end)?;
        for lane in &mut track.effects {
            tidy_lane(lane, p.loop_end)?;
        }
    }
    for lane in &mut p.mix_effects {
        tidy_lane(lane, p.loop_end)?;
    }
    measure(p);
    Ok(())
}

/// The lanes holding `lane`, under its track or under the mix, and its index there.
fn lanes_of(p: &mut Project, lane: LaneId) -> Result<(&mut Vec<EffectLane>, usize)> {
    let lists = p
        .tracks
        .iter_mut()
        .map(|t| &mut t.effects)
        .chain(std::iter::once(&mut p.mix_effects));
    for list in lists {
        if let Some(i) = list.iter().position(|l| l.id == lane) {
            return Ok((list, i));
        }
    }
    Err(Error::Invalid("no such effect lane".into()))
}

fn lane_mut(p: &mut Project, lane: LaneId) -> Result<&mut EffectLane> {
    let (list, i) = lanes_of(p, lane)?;
    list.get_mut(i)
        .ok_or_else(|| Error::Invalid("no such effect lane".into()))
}

fn range_mut(p: &mut Project, range: RangeId) -> Result<&mut EffectRange> {
    let lanes = p
        .tracks
        .iter_mut()
        .flat_map(|t| &mut t.effects)
        .chain(&mut p.mix_effects);
    let mut ranges = lanes.flat_map(|l| &mut l.ranges);
    ranges
        .find(|r| r.id == range)
        .ok_or_else(|| Error::Invalid("no such effect range".into()))
}

fn apply(p: &mut Project, command: Command) -> Result<()> {
    let wrap = p.loop_end;
    match command {
        Command::AddTrack { name } => {
            let id = TrackId(p.new_id());
            p.tracks.push(Track {
                id,
                name,
                gain_db: 0.0,
                muted: false,
                clips: vec![],
                effects: vec![],
            });
        }
        Command::RemoveTrack { track } => {
            let before = p.tracks.len();
            p.tracks.retain(|t| t.id != track);
            if p.tracks.len() == before {
                return Err(Error::NoTrack(track));
            }
        }
        Command::SetTrackGain { track, db } => track_mut(p, track)?.gain_db = check_db(db)?,
        Command::SetTrackMute { track, muted } => track_mut(p, track)?.muted = muted,
        Command::AddClip { track, source, start } => {
            let len = source.frames();
            if len <= 0 {
                return Err(Error::EmptySource);
            }
            if p.is_empty() && p.sample_rate != source.sample_rate {
                // A project without audio takes the rate of its first clip, also after all clips were deleted.
                if let Some(l) = p.loop_end
                    && p.sample_rate > 0
                {
                    let seconds = l as f64 / f64::from(p.sample_rate);
                    p.loop_end = Some(((seconds * f64::from(source.sample_rate)).round() as Frames).max(1));
                }
                p.sample_rate = source.sample_rate;
            }
            let source = if source.sample_rate == p.sample_rate {
                source
            } else {
                Arc::new(resample(&source, p.sample_rate)?)
            };
            let len = source.frames();
            let id = ClipId(p.new_id());
            let clip = Clip {
                id,
                source,
                start: check_placement(start, len)?,
                offset: 0,
                len,
                fade_in: Fade::default(),
                fade_out: Fade::default(),
                crossfade_shape: FadeShape::EqualPower,
                gain: vec![],
                channels: ChannelMode::Stereo,
            };
            track_mut(p, track)?.clips.push(clip);
        }
        Command::MoveClip { clip, track, start } => {
            track_mut(p, track)?;
            let mut moved = take_clip(p, clip)?;
            moved.start = check_placement(start, moved.len)?;
            track_mut(p, track)?.clips.push(moved);
        }
        Command::MoveClipToNewTrack { clip, start } => {
            let mut moved = take_clip(p, clip)?;
            moved.start = check_placement(start, moved.len)?;
            let id = TrackId(p.new_id());
            let name = moved.source.name.clone();
            p.tracks.push(Track {
                id,
                name,
                gain_db: 0.0,
                muted: false,
                clips: vec![moved],
                effects: vec![],
            });
        }
        Command::TrimClip { clip, start, end } => clip_mut(p, clip)?.trim(start, end)?,
        Command::SplitClip { clip, at } => split(p, clip, at)?,
        Command::AddEffectLane { track, effect } => {
            let lane = EffectLane::new(LaneId(p.new_id()), effect)?;
            match track {
                Some(track) => track_mut(p, track)?.effects.push(lane),
                None => p.mix_effects.push(lane),
            }
        }
        Command::SetEffect { range, effect } => {
            let lanes = p
                .tracks
                .iter_mut()
                .flat_map(|t| &mut t.effects)
                .chain(&mut p.mix_effects);
            let mut lanes = lanes.filter(|l| l.ranges.iter().any(|r| r.id == range));
            let lane = lanes
                .next()
                .ok_or_else(|| Error::Invalid("no such effect range".into()))?;
            lane.set_range_effect(Some(range), effect)?;
        }
        Command::SetLaneEffect { lane, effect } => lane_mut(p, lane)?.set_effect(effect)?,
        Command::SetEffectEnabled { lane, enabled } => lane_mut(p, lane)?.enabled = enabled,
        Command::MoveEffectLane { lane, down } => {
            let (list, i) = lanes_of(p, lane)?;
            let j = if down { i.saturating_add(1) } else { i.saturating_sub(1) };
            if j >= list.len() || j == i {
                return Err(Error::Invalid(format!(
                    "the effect is already {}",
                    if down { "last" } else { "first" }
                )));
            }
            list.swap(i, j);
        }
        Command::RemoveEffectLane { lane } => {
            let (list, i) = lanes_of(p, lane)?;
            list.remove(i);
        }
        Command::AddEffectRange { lane, start, end } => {
            let id = RangeId(p.new_id());
            let lane = lane_mut(p, lane)?;
            lane.ranges.push(EffectRange {
                id,
                start,
                end,
                effect: lane.effect,
                gain: 1.0,
            });
        }
        Command::SetEffectRange { range, start, end } => {
            let r = range_mut(p, range)?;
            (r.start, r.end) = (start, end);
        }
        Command::RemoveEffectRange { range } => {
            let lanes = p
                .tracks
                .iter_mut()
                .flat_map(|t| &mut t.effects)
                .chain(&mut p.mix_effects);
            let mut found = false;
            for lane in lanes {
                let before = lane.ranges.len();
                lane.ranges.retain(|r| r.id != range);
                found |= lane.ranges.len() != before;
            }
            if !found {
                return Err(Error::Invalid("no such effect range".into()));
            }
        }
        Command::DuplicateClip { clip, track, start } => {
            let mut copy = p.clip(clip).map(|(_, c)| c.clone()).ok_or(Error::NoClip(clip))?;
            copy.id = ClipId(p.new_id());
            copy.start = check_placement(start, copy.len)?;
            match track {
                Some(track) => track_mut(p, track)?.clips.push(copy),
                None => {
                    let id = TrackId(p.new_id());
                    let name = copy.source.name.clone();
                    p.tracks.push(Track {
                        id,
                        name,
                        gain_db: 0.0,
                        muted: false,
                        clips: vec![copy],
                        effects: vec![],
                    });
                }
            }
        }
        Command::DeleteClip { clip } => {
            take_clip(p, clip)?;
        }
        Command::SetFadeIn { clip, fade } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_before(clips, i, wrap) > 0 {
                return Err(Error::Invalid(
                    "the start of this clip is a crossfade; move the clip to change it".into(),
                ));
            }
            let out_len = edge_fades(clips, i, wrap).1.len;
            let c = clip_mut(p, clip)?;
            check_fades(c.len, fade.len, out_len)?;
            c.fade_in = fade;
        }
        Command::SetFadeOut { clip, fade } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_after(clips, i, wrap) > 0 {
                return Err(Error::Invalid(
                    "the end of this clip is a crossfade; move the clip to change it".into(),
                ));
            }
            let in_len = edge_fades(clips, i, wrap).0.len;
            let c = clip_mut(p, clip)?;
            check_fades(c.len, in_len, fade.len)?;
            c.fade_out = fade;
        }
        Command::SetCrossfadeShape { clip, shape } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_before(clips, i, wrap) == 0 {
                return Err(Error::Invalid("no other clip overlaps the start of this clip".into()));
            }
            clip_mut(p, clip)?.crossfade_shape = shape;
        }
        Command::SetLoopEnd { at } => {
            if at.is_some_and(|l| l <= 0) {
                return Err(Error::Invalid("the loop end must be after 0".into()));
            }
            p.loop_end = at;
        }
        Command::MatchCrossfade { clip, range } => match_crossfade(p, clip, range)?,
        Command::SetSampleRate { rate } => set_sample_rate(p, rate)?,
        Command::SetChannels { clips, mode } => {
            for clip in clips {
                clip_mut(p, clip)?.channels = mode;
            }
        }
        Command::SetGainCurve { clip, mut points } => {
            for point in &mut points {
                point.db = check_db(point.db)?;
            }
            points.sort_by_key(|point| point.at);
            clip_mut(p, clip)?.gain = points;
        }
    }
    Ok(())
}

/// Sorts a track's clips, refuses layouts where an overlap is not a clear crossfade, and drops or
/// shortens free fades that no longer fit. With a loop, the checks go round the seam too.
fn tidy(track: &mut Track, loop_end: Option<Frames>) -> Result<()> {
    track.clips.sort_by_key(|c| c.start);
    // The clips in order round the circle: the last one a loop earlier, all, the first one a loop later.
    let mut ring: Vec<(Frames, Frames)> = track.clips.iter().map(|c| (c.start, c.end())).collect();
    if let Some(l) = loop_end
        && let (Some(&(first_start, first_end)), Some(&(last_start, last_end))) = (ring.first(), ring.last())
    {
        if first_start < 0 || last_start >= l {
            return Err(Error::Invalid(
                "with the loop on, every clip must start between 0 and the loop end".into(),
            ));
        }
        if track.clips.iter().any(|c| c.len > l.saturating_mul(2)) {
            return Err(Error::Invalid(
                "with the loop on, a clip can be at most two loops long, or its end would play over itself; \
                 turn the loop off or move the loop end first"
                    .into(),
            ));
        }
        ring.insert(0, (last_start - l, last_end.saturating_sub(l)));
        ring.push((first_start + l, first_end.saturating_add(l)));
    }
    for pair in ring.windows(2) {
        if let [a, b] = pair
            && (b.0 == a.0 || b.1 <= a.1)
        {
            return Err(Error::Invalid(
                "a clip cannot lie completely inside another clip on the same track".into(),
            ));
        }
    }
    for triple in ring.windows(3) {
        if let [a, _, c] = triple
            && c.0 < a.1
        {
            return Err(Error::Invalid(
                "at most two clips can overlap at any point of a track".into(),
            ));
        }
    }
    for i in 0..track.clips.len() {
        let before = overlap_before(&track.clips, i, loop_end);
        let after = overlap_after(&track.clips, i, loop_end);
        let Some(c) = track.clips.get_mut(i) else { continue };
        if before > 0 {
            c.fade_in.len = 0;
        }
        if after > 0 {
            c.fade_out.len = 0;
        }
        let out_len = after.max(c.fade_out.len);
        c.fade_in.len = c.fade_in.len.min(c.len - out_len).max(0);
        let in_len = before.max(c.fade_in.len);
        c.fade_out.len = c.fade_out.len.min(c.len - in_len).max(0);
    }
    Ok(())
}

fn set_sample_rate(p: &mut Project, rate: u32) -> Result<()> {
    if !(8000..=384_000).contains(&rate) {
        return Err(Error::Invalid(
            "the sample rate must be between 8000 and 384000 Hz".into(),
        ));
    }
    let old = p.sample_rate;
    if old == rate {
        return Ok(());
    }
    let scale = |f: Frames| {
        if old == 0 {
            f
        } else {
            (f as f64 * f64::from(rate) / f64::from(old)).round() as Frames
        }
    };
    // Keyed by the old source, which the map keeps alive so its address is not reused meanwhile.
    let mut converted: HashMap<usize, (Arc<Source>, Arc<Source>)> = HashMap::new();
    // Converted audio is read from where its file is now, so the old relocations are no longer needed.
    let relocated = std::mem::take(&mut p.relocated);
    for clip in p.tracks.iter_mut().flat_map(|t| t.clips.iter_mut()) {
        let key = Arc::as_ptr(&clip.source) as usize;
        let source = match converted.get(&key) {
            Some((_, new)) => new.clone(),
            None => {
                let moved = relocated.iter().find(|(s, _)| Arc::ptr_eq(s, &clip.source));
                let file = moved.map(|(_, f)| f.as_path()).or(clip.source.path.as_deref());
                let original = match file {
                    Some(path) => Arc::new(
                        load_source(path)
                            .map_err(|e| Error::Invalid(format!("cannot read {} again: {e}", path.display())))?,
                    ),
                    None => clip.source.clone(),
                };
                let new = Arc::new(resample(&original, rate)?);
                converted.insert(key, (clip.source.clone(), new.clone()));
                new
            }
        };
        let frames = source.frames();
        if frames <= 0 {
            return Err(Error::EmptySource);
        }
        clip.offset = scale(clip.offset).clamp(0, frames - 1);
        clip.len = scale(clip.len).clamp(1, frames - clip.offset);
        clip.start = scale(clip.start);
        clip.fade_in.len = scale(clip.fade_in.len);
        clip.fade_out.len = scale(clip.fade_out.len);
        for point in &mut clip.gain {
            point.at = scale(point.at);
        }
        clip.source = source;
    }
    p.loop_end = p.loop_end.map(|l| scale(l).max(1));
    let lanes = p
        .tracks
        .iter_mut()
        .flat_map(|t| &mut t.effects)
        .chain(&mut p.mix_effects);
    for range in lanes.flat_map(|l| &mut l.ranges) {
        range.start = scale(range.start);
        range.end = scale(range.end).max(range.start.saturating_add(1));
    }
    p.sample_rate = rate;
    Ok(())
}

/// Above this waveform match, a linear crossfade keeps the level even; below it, equal power does.
const LINEAR_FROM_MATCH: f32 = 0.5;

/// Tries every shift of the later clip's audio against the earlier one within `range` and keeps the best.
/// The earlier clip's end moves along, so the crossfade keeps its length where that clip has audio for it.
fn match_crossfade(p: &mut Project, clip: ClipId, range: Frames) -> Result<()> {
    let wrap = p.loop_end;
    let (clips, i) = locate(p, clip)?;
    let later = clips.get(i).ok_or(Error::NoClip(clip))?.clone();
    // At the seam the earlier clip is the last one, a loop earlier.
    let (earlier_index, loop_shift) = match (i.checked_sub(1), wrap) {
        (Some(j), _) => (j, 0),
        (None, Some(l)) => (clips.len() - 1, l),
        (None, None) => return Err(Error::Invalid("no other clip overlaps the start of this clip".into())),
    };
    let mut earlier = clips.get(earlier_index).ok_or(Error::NoClip(clip))?.clone();
    earlier.start = earlier.start.saturating_sub(loop_shift);
    if earlier.end() <= later.start {
        return Err(Error::Invalid("no other clip overlaps the start of this clip".into()));
    }
    let room = earlier.hidden().1;
    let shape = later.crossfade_shape;
    let score = |shift: Frames, max_points: Frames| {
        let mut a = earlier.clone();
        a.len = a.len.saturating_add(shift.min(room));
        let mut b = later.clone();
        b.start = b.start.saturating_add(shift);
        let len = a.end().saturating_sub(b.start);
        if a.len <= 0 || len <= 0 {
            return None;
        }
        let fade = Fade { len, shape };
        similarity_sampled(
            (&a, (Fade::default(), fade)),
            (&b, (fade, Fade::default())),
            b.start,
            a.end(),
            max_points,
        )
    };
    // A quick pass over every shift, then the few best measured in full.
    let range = range.clamp(0, 1_000_000);
    let mut rough: Vec<(f32, Frames)> = (-range..=range).filter_map(|s| Some((score(s, 2000)?, s))).collect();
    rough.sort_by(|x, y| y.0.total_cmp(&x.0));
    let best = rough
        .iter()
        .take(5)
        .filter_map(|&(_, s)| Some((score(s, 200_000)?, s)))
        .max_by(|x, y| x.0.total_cmp(&y.0).then(y.1.abs().cmp(&x.1.abs())));
    let Some((value, shift)) = best else {
        return Err(Error::Invalid(
            "the crossfade is silent, so there is nothing to match".into(),
        ));
    };

    let earlier_id = earlier.id;
    let c = clip_mut(p, earlier_id)?;
    let (start, end) = (c.start, c.end().saturating_add(shift.min(room)));
    c.trim(start, end)?;
    if loop_shift > 0 {
        p.loop_end = Some(loop_shift.saturating_add(shift));
    } else {
        let c = clip_mut(p, clip)?;
        c.start = check_placement(c.start.saturating_add(shift), c.len)?;
    }
    clip_mut(p, clip)?.crossfade_shape = if value > LINEAR_FROM_MATCH {
        FadeShape::Linear
    } else {
        FadeShape::EqualPower
    };
    Ok(())
}

fn split(p: &mut Project, clip: ClipId, at: Frames) -> Result<()> {
    let id = ClipId(p.new_id());
    let wrap = p.loop_end;
    let (clips, i) = locate(p, clip)?;
    let (fade_in, fade_out) = edge_fades(clips, i, wrap);
    let c = clips.get(i).ok_or(Error::NoClip(clip))?.clone();
    let cut = at.saturating_sub(c.start);
    if cut <= 0 || cut >= c.len {
        return Err(Error::Invalid("the split point is outside the clip".into()));
    }
    if cut < fade_in.len || c.len - cut < fade_out.len {
        return Err(Error::Invalid("the split point is inside a fade or crossfade".into()));
    }
    // Both halves get a point at the cut so their curves continue exactly where the cut was.
    let at_cut = GainPoint {
        at: cut,
        db: envelope_db(&c.gain, cut),
    };
    let mut left = c.clone();
    left.len = cut;
    left.fade_out = Fade::default();
    left.gain.retain(|g| g.at < cut);
    left.gain.push(at_cut);
    let mut right = c.clone();
    right.id = id;
    right.start = at;
    right.offset = c.offset.saturating_add(cut);
    right.len = c.len - cut;
    right.fade_in = Fade::default();
    right.gain.retain(|g| g.at > cut);
    right.gain.insert(0, at_cut);
    for point in &mut right.gain {
        point.at -= cut;
    }
    let track = track_of(p, clip)?;
    *clip_mut(p, clip)? = left;
    track_mut(p, track)?.clips.push(right);
    Ok(())
}

/// The clip's track's clips and its index among them.
fn locate(p: &Project, id: ClipId) -> Result<(&[Clip], usize)> {
    p.tracks
        .iter()
        .find_map(|t| t.clips.iter().position(|c| c.id == id).map(|i| (t.clips.as_slice(), i)))
        .ok_or(Error::NoClip(id))
}

fn track_of(p: &Project, id: ClipId) -> Result<TrackId> {
    p.clip(id).map(|(t, _)| t.id).ok_or(Error::NoClip(id))
}

fn track_mut(p: &mut Project, id: TrackId) -> Result<&mut Track> {
    p.tracks.iter_mut().find(|t| t.id == id).ok_or(Error::NoTrack(id))
}

fn clip_mut(p: &mut Project, id: ClipId) -> Result<&mut Clip> {
    p.tracks
        .iter_mut()
        .flat_map(|t| &mut t.clips)
        .find(|c| c.id == id)
        .ok_or(Error::NoClip(id))
}

fn take_clip(p: &mut Project, id: ClipId) -> Result<Clip> {
    for track in &mut p.tracks {
        if let Some(i) = track.clips.iter().position(|c| c.id == id) {
            return Ok(track.clips.remove(i));
        }
    }
    Err(Error::NoClip(id))
}

/// A clip may start before 0 (that part is not played), but some of it must remain after 0.
fn check_placement(start: Frames, len: Frames) -> Result<Frames> {
    if start.saturating_add(len) <= 0 {
        return Err(Error::Invalid("the clip would end before 0".into()));
    }
    Ok(start)
}

fn check_fades(len: Frames, fade_in: Frames, fade_out: Frames) -> Result<()> {
    if fade_in < 0 || fade_out < 0 || fade_in.saturating_add(fade_out) > len {
        return Err(Error::Invalid("the fades are longer than the clip".into()));
    }
    Ok(())
}

pub(crate) fn check_db(db: f32) -> Result<f32> {
    if db.is_nan() {
        return Err(Error::Invalid("volume is not a number".into()));
    }
    Ok(db.clamp(MIN_DB, MAX_DB))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render;

    fn ramp() -> Arc<Source> {
        let samples: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
        Arc::new(Source {
            path: None,
            name: "ramp".into(),
            sample_rate: 1000,
            channels: vec![samples],
        })
    }

    /// One track with a 1000-frame clip at 100.
    fn ramp_session() -> (Session, TrackId, ClipId) {
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source: ramp(),
            start: 100,
        })
        .unwrap();
        let clip = s.project().tracks[0].clips[0].id;
        (s, track, clip)
    }

    fn add(s: &mut Session, track: TrackId, start: Frames) -> Result<ClipId> {
        s.execute(Command::AddClip {
            track,
            source: ramp(),
            start,
        })?;
        Ok(s.project().tracks[0]
            .clips
            .iter()
            .find(|c| c.start == start)
            .unwrap()
            .id)
    }

    #[test]
    fn audio_past_the_loop_end_crossfades_into_the_start() {
        let steady = Arc::new(Source {
            path: None,
            name: "steady".into(),
            sample_rate: 1000,
            channels: vec![vec![1.0; 1500]],
        });
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source: steady,
            start: 0,
        })
        .unwrap();
        let clip = s.project().tracks[0].clips[0].id;
        s.execute(Command::SetLoopEnd { at: Some(1000) }).unwrap();
        s.execute(Command::SetCrossfadeShape {
            clip,
            shape: FadeShape::Linear,
        })
        .unwrap();

        assert_eq!(s.project().len(), 1000);
        let [left, _] = render(s.project(), 0, 2000);
        for (i, v) in left.iter().enumerate() {
            assert!((v - 1.0).abs() < 1e-6, "frame {i}: {v}");
        }
        assert_eq!(render(s.project(), 0, 1000), render(s.project(), 1000, 1000));

        // A clip longer than two loops would cover the seam three times; a clip after the loop end never plays.
        assert!(s.execute(Command::SetLoopEnd { at: Some(700) }).is_err());
        assert!(
            s.execute(Command::MoveClip {
                clip,
                track,
                start: 1000
            })
            .is_err()
        );
        s.execute(Command::SetLoopEnd { at: None }).unwrap();
        assert_eq!(s.project().len(), 1500);
    }

    #[test]
    fn matching_a_crossfade_brings_the_waves_in_step_and_keeps_its_length() {
        // Repeats every 20 frames, like the cycle of an engine.
        let cycle = Arc::new(Source {
            path: None,
            name: "cycle".into(),
            sample_rate: 1000,
            channels: vec![
                (0..1500)
                    .map(|i| (i as f32 * std::f32::consts::TAU / 20.0).sin())
                    .collect(),
            ],
        });
        let in_step = |p: &Project| {
            let clips = &p.tracks[0].clips;
            let fades = |i| edge_fades(clips, i, p.loop_end);
            let (a, b) = match p.loop_end {
                Some(l) => {
                    let mut a = clips[clips.len() - 1].clone();
                    a.start -= l;
                    (a, clips[0].clone())
                }
                None => (clips[0].clone(), clips[1].clone()),
            };
            let last = clips.len() - 1;
            let (fa, fb) = if p.loop_end.is_some() {
                (fades(last), fades(0))
            } else {
                (fades(0), fades(1))
            };
            crate::similarity((&a, fa), (&b, fb), b.start, a.end()).unwrap()
        };

        // The loop seam: 7 frames out of step.
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source: cycle.clone(),
            start: 0,
        })
        .unwrap();
        let clip = s.project().tracks[0].clips[0].id;
        s.execute(Command::SetLoopEnd { at: Some(1007) }).unwrap();
        assert!(in_step(s.project()) < 0.5);
        s.execute(Command::MatchCrossfade { clip, range: 50 }).unwrap();
        let l = s.project().loop_end.unwrap();
        assert_eq!(l % 20, 0, "loop end {l}");
        assert_eq!(overlap_before(&s.project().tracks[0].clips, 0, Some(l)), 1500 - 1007);
        assert!(in_step(s.project()) > 0.999);
        assert_eq!(s.project().tracks[0].clips[0].crossfade_shape, FadeShape::Linear);

        // Two clips: the later one 13 frames out of step; the earlier one has hidden audio to grow into.
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source: cycle.clone(),
            start: 0,
        })
        .unwrap();
        let a = s.project().tracks[0].clips[0].id;
        s.execute(Command::TrimClip {
            clip: a,
            start: 0,
            end: 1000,
        })
        .unwrap();
        s.execute(Command::AddClip {
            track,
            source: cycle,
            start: 813,
        })
        .unwrap();
        let b = s.project().tracks[0].clips[1].id;
        s.execute(Command::MatchCrossfade { clip: b, range: 50 }).unwrap();
        let clips = &s.project().tracks[0].clips;
        assert_eq!(clips[1].start % 20, 0, "start {}", clips[1].start);
        assert_eq!(clips[0].end() - clips[1].start, 1000 - 813);
        assert!(in_step(s.project()) > 0.999);
    }

    /// Regression: deleting all audio kept the old sample rate, so audio at another rate was refused.
    #[test]
    fn an_emptied_project_takes_the_rate_of_new_audio() {
        let (mut s, track, clip) = ramp_session();
        s.execute(Command::SetLoopEnd { at: Some(500) }).unwrap();
        s.execute(Command::DeleteClip { clip }).unwrap();
        let faster = Arc::new(Source {
            path: None,
            name: "faster".into(),
            sample_rate: 2000,
            channels: vec![vec![0.1; 1500]],
        });
        s.execute(Command::AddClip {
            track,
            source: faster,
            start: 0,
        })
        .unwrap();
        assert_eq!(s.project().sample_rate, 2000);
        assert_eq!(s.project().loop_end, Some(1000), "the loop keeps its length in seconds");
    }

    #[test]
    fn audio_at_another_rate_is_converted_and_the_rate_can_change() {
        let at = |rate: u32, frames: usize| {
            Arc::new(Source {
                path: None,
                name: format!("{rate}"),
                sample_rate: rate,
                channels: vec![vec![0.25; frames]],
            })
        };
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source: at(8000, 8000),
            start: 0,
        })
        .unwrap();
        // One second at 16 kHz becomes one second at the project's 8 kHz.
        s.execute(Command::AddClip {
            track,
            source: at(16_000, 16_000),
            start: 8000,
        })
        .unwrap();
        let second = &s.project().tracks[0].clips[1];
        assert_eq!((second.source.sample_rate, second.len), (8000, 8000));

        s.execute(Command::SetLoopEnd { at: Some(12_000) }).unwrap();
        s.execute(Command::SetSampleRate { rate: 16_000 }).unwrap();
        let p = s.project();
        assert_eq!(p.sample_rate, 16_000);
        let spans: Vec<_> = p.tracks[0].clips.iter().map(|c| (c.start, c.len)).collect();
        assert_eq!(spans, [(0, 16_000), (16_000, 16_000)]);
        assert_eq!(p.loop_end, Some(24_000));
        s.undo().unwrap();
        assert_eq!(s.project().sample_rate, 8000);
    }

    #[test]
    fn a_trim_hides_audio_that_widening_brings_back() {
        let (mut s, _, clip) = ramp_session();
        let points = vec![GainPoint { at: 0, db: 0.0 }, GainPoint { at: 1000, db: -20.0 }];
        s.execute(Command::SetGainCurve { clip, points }).unwrap();
        let before = render(s.project(), 0, 1200);

        s.execute(Command::TrimClip {
            clip,
            start: 300,
            end: 700,
        })
        .unwrap();
        let trimmed = render(s.project(), 0, 1200);
        for (i, (t, b)) in trimmed[0].iter().zip(&before[0]).enumerate() {
            let kept = (300..700).contains(&i);
            assert_eq!(*t, if kept { *b } else { 0.0 }, "frame {i}");
        }
        assert_eq!(s.project().tracks[0].clips[0].hidden(), (200, 400));

        s.execute(Command::TrimClip {
            clip,
            start: 100,
            end: 1100,
        })
        .unwrap();
        assert_eq!(render(s.project(), 0, 1200), before);
        assert_eq!(s.project().tracks[0].clips[0].hidden(), (0, 0));
        assert!(
            s.execute(Command::TrimClip {
                clip,
                start: 99,
                end: 1100
            })
            .is_err()
        );
        assert!(
            s.execute(Command::TrimClip {
                clip,
                start: 100,
                end: 1101
            })
            .is_err()
        );
    }

    #[test]
    fn split_does_not_change_the_sound() {
        let (mut s, _, clip) = ramp_session();
        let points = vec![GainPoint { at: 0, db: 0.0 }, GainPoint { at: 1000, db: -30.0 }];
        s.execute(Command::SetGainCurve { clip, points }).unwrap();
        s.execute(Command::SetFadeIn {
            clip,
            fade: Fade {
                len: 100,
                shape: FadeShape::EqualPower,
            },
        })
        .unwrap();
        let before = render(s.project(), 0, 1200);

        s.execute(Command::SplitClip { clip, at: 600 }).unwrap();

        assert_eq!(s.project().tracks[0].clips.len(), 2);
        let after = render(s.project(), 0, 1200);
        for (i, (a, b)) in before[0].iter().zip(&after[0]).enumerate() {
            assert!((a - b).abs() < 1e-6, "frame {i}: {a} vs {b}");
        }
    }

    #[test]
    fn failed_command_changes_nothing_and_undo_restores() {
        let (mut s, track, clip) = ramp_session();
        let undo_depth = s.undo.len();

        let too_long = Fade {
            len: 5000,
            shape: FadeShape::Linear,
        };
        assert!(s.execute(Command::SetFadeIn { clip, fade: too_long }).is_err());
        assert_eq!(s.undo.len(), undo_depth);

        s.execute(Command::MoveClip { clip, track, start: 0 }).unwrap();
        assert_eq!(s.project().tracks[0].clips[0].start, 0);
        s.undo().unwrap();
        assert_eq!(s.project().tracks[0].clips[0].start, 100);
        s.redo().unwrap();
        assert_eq!(s.project().tracks[0].clips[0].start, 0);
    }

    #[test]
    fn moving_into_overlap_replaces_the_fades_with_a_crossfade() {
        let (mut s, track, a) = ramp_session();
        s.execute(Command::SetFadeOut {
            clip: a,
            fade: Fade {
                len: 300,
                shape: FadeShape::Linear,
            },
        })
        .unwrap();
        let b = add(&mut s, track, 2000).unwrap();
        s.execute(Command::SetFadeIn {
            clip: b,
            fade: Fade {
                len: 50,
                shape: FadeShape::Linear,
            },
        })
        .unwrap();

        s.execute(Command::MoveClip {
            clip: b,
            track,
            start: 900,
        })
        .unwrap();

        let clips = &s.project().tracks[0].clips;
        assert_eq!((clips[0].fade_out.len, clips[1].fade_in.len), (0, 0));
        assert_eq!(
            edge_fades(clips, 0, None).1,
            Fade {
                len: 200,
                shape: FadeShape::EqualPower
            }
        );
        assert_eq!(
            edge_fades(clips, 1, None).0,
            Fade {
                len: 200,
                shape: FadeShape::EqualPower
            }
        );
        let err = s
            .execute(Command::SetFadeIn {
                clip: b,
                fade: Fade::default(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("crossfade"), "{err}");
    }

    #[test]
    fn a_clip_moved_before_zero_plays_only_its_rest_and_can_come_back() {
        let (mut s, track, clip) = ramp_session();
        s.execute(Command::MoveClip {
            clip,
            track,
            start: -400,
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 600);
        // Frame 0 plays source frame 400 of the ramp.
        assert!((left[0] - 0.4).abs() < 1e-6, "{}", left[0]);
        assert_eq!(s.project().len(), 600);

        assert!(
            s.execute(Command::MoveClip {
                clip,
                track,
                start: -1000
            })
            .is_err()
        );
        s.execute(Command::MoveClip { clip, track, start: 0 }).unwrap();
        assert_eq!(render(s.project(), 0, 1)[0][0], 0.0);
    }

    #[test]
    fn moving_to_a_new_track_is_one_undo_step() {
        let (mut s, _, clip) = ramp_session();
        s.execute(Command::MoveClipToNewTrack { clip, start: 50 }).unwrap();
        let tracks = &s.project().tracks;
        assert_eq!(
            (tracks.len(), tracks[0].clips.len(), tracks[1].clips[0].start),
            (2, 0, 50)
        );
        assert_eq!(tracks[1].name, "ramp");

        s.undo().unwrap();
        assert_eq!(s.project().tracks.len(), 1);
        assert_eq!(s.project().tracks[0].clips[0].start, 100);
    }

    #[test]
    fn unclear_overlaps_are_refused() {
        let (mut s, track, _) = ramp_session();
        // Same start and length as the clip at 100..1100, so fully inside it.
        assert!(add(&mut s, track, 100).is_err());
        let b = add(&mut s, track, 900).unwrap();
        // 1000 is covered by 100..1100, 900..1900 and the new clip.
        assert!(add(&mut s, track, 1000).is_err());
        // Moving b fully over the first clip is refused too, and nothing changes.
        let before = s.project().clone();
        assert!(
            s.execute(Command::MoveClip {
                clip: b,
                track,
                start: 100
            })
            .is_err()
        );
        assert!(Arc::ptr_eq(&before, s.project()));
    }
}
