use std::sync::Arc;

use crate::mix::{edge_fades, envelope_db, overlap_after, overlap_before};
use crate::{
    Clip, ClipId, Error, Fade, FadeShape, Frames, GainPoint, MAX_DB, MIN_DB, Project, Result, Source, Track, TrackId,
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
        for track in &mut project.tracks {
            tidy(track)?;
        }
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
        for track in &mut next.tracks {
            tidy(track)?;
        }
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
}

fn apply(p: &mut Project, command: Command) -> Result<()> {
    match command {
        Command::AddTrack { name } => {
            let id = TrackId(p.new_id());
            p.tracks.push(Track {
                id,
                name,
                gain_db: 0.0,
                muted: false,
                clips: vec![],
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
            if p.sample_rate == 0 {
                p.sample_rate = source.sample_rate;
            } else if p.sample_rate != source.sample_rate {
                return Err(Error::SampleRateMismatch {
                    project: p.sample_rate,
                    found: source.sample_rate,
                });
            }
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
            });
        }
        Command::TrimClip { clip, start, end } => clip_mut(p, clip)?.trim(start, end)?,
        Command::SplitClip { clip, at } => split(p, clip, at)?,
        Command::DeleteClip { clip } => {
            take_clip(p, clip)?;
        }
        Command::SetFadeIn { clip, fade } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_before(clips, i) > 0 {
                return Err(Error::Invalid(
                    "the start of this clip is a crossfade; move the clip to change it".into(),
                ));
            }
            let out_len = edge_fades(clips, i).1.len;
            let c = clip_mut(p, clip)?;
            check_fades(c.len, fade.len, out_len)?;
            c.fade_in = fade;
        }
        Command::SetFadeOut { clip, fade } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_after(clips, i) > 0 {
                return Err(Error::Invalid(
                    "the end of this clip is a crossfade; move the clip to change it".into(),
                ));
            }
            let in_len = edge_fades(clips, i).0.len;
            let c = clip_mut(p, clip)?;
            check_fades(c.len, in_len, fade.len)?;
            c.fade_out = fade;
        }
        Command::SetCrossfadeShape { clip, shape } => {
            let (clips, i) = locate(p, clip)?;
            if overlap_before(clips, i) == 0 {
                return Err(Error::Invalid("no other clip overlaps the start of this clip".into()));
            }
            clip_mut(p, clip)?.crossfade_shape = shape;
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
/// shortens free fades that no longer fit.
fn tidy(track: &mut Track) -> Result<()> {
    track.clips.sort_by_key(|c| c.start);
    for pair in track.clips.windows(2) {
        if let [a, b] = pair
            && (b.start == a.start || b.end() <= a.end())
        {
            return Err(Error::Invalid(
                "a clip cannot lie completely inside another clip on the same track".into(),
            ));
        }
    }
    for triple in track.clips.windows(3) {
        if let [a, _, c] = triple
            && c.start < a.end()
        {
            return Err(Error::Invalid(
                "at most two clips can overlap at any point of a track".into(),
            ));
        }
    }
    for i in 0..track.clips.len() {
        let before = overlap_before(&track.clips, i);
        let after = overlap_after(&track.clips, i);
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

fn split(p: &mut Project, clip: ClipId, at: Frames) -> Result<()> {
    let id = ClipId(p.new_id());
    let (clips, i) = locate(p, clip)?;
    let (fade_in, fade_out) = edge_fades(clips, i);
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
            edge_fades(clips, 0).1,
            Fade {
                len: 200,
                shape: FadeShape::EqualPower
            }
        );
        assert_eq!(
            edge_fades(clips, 1).0,
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
