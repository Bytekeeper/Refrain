//! The project: tracks, clips, the audio they play, effect lanes and the loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{EffectLane, LaneId};

/// A position or length in sample frames at the project sample rate.
pub type Frames = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClipId(pub u64);

/// Decoded audio from one file. Clips share it, so edits never copy samples.
#[derive(Debug)]
pub struct Source {
    /// The file it was read from; `None` for audio made in memory, which cannot be saved.
    pub path: Option<PathBuf>,
    pub name: String,
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
    /// For sound the editor made: how, so it can be made again for another length, loop or rate.
    pub generated: Option<crate::Generated>,
}

impl Source {
    pub fn frames(&self) -> Frames {
        let shortest = self.channels.iter().map(Vec::len).min().unwrap_or(0);
        Frames::try_from(shortest).unwrap_or(Frames::MAX)
    }

    pub fn sample(&self, channel: usize, frame: Frames) -> f32 {
        let Ok(frame) = usize::try_from(frame) else { return 0.0 };
        self.channels
            .get(channel)
            .and_then(|c| c.get(frame))
            .copied()
            .unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeShape {
    #[default]
    Linear,
    /// Keeps the loudness constant across a crossfade of unrelated material.
    EqualPower,
}

/// Which channels of its audio a clip plays. A mono recording plays the same on both sides either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMode {
    #[default]
    Stereo,
    /// Both channels mixed, on both sides.
    Mono,
    /// Only the left channel, on both sides; for a recording with a broken right channel.
    Left,
    /// Only the right channel, on both sides.
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Fade {
    pub len: Frames,
    pub shape: FadeShape,
}

/// One point of a clip's volume curve. `at` counts from the clip start.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GainPoint {
    pub at: Frames,
    pub db: f32,
}

#[derive(Debug, Clone)]
pub struct Clip {
    pub id: ClipId,
    pub source: Arc<Source>,
    /// Timeline position of the first frame. May be negative; nothing before 0 is played.
    pub start: Frames,
    /// First used frame inside the source.
    pub offset: Frames,
    pub len: Frames,
    /// Only used at an end that no other clip overlaps; an overlap is always a crossfade.
    pub fade_in: Fade,
    pub fade_out: Fade,
    /// Curve of the crossfade where the previous clip overlaps this clip's start.
    pub crossfade_shape: FadeShape,
    /// Sorted by `at`. Empty means 0 dB.
    pub gain: Vec<GainPoint>,
    pub channels: ChannelMode,
}

impl Clip {
    /// Left and right of source frame `frame`, as this clip plays them.
    pub fn frame(&self, frame: Frames) -> (f32, f32) {
        let left = self.source.sample(0, frame);
        if self.source.channels.len() < 2 {
            return (left, left);
        }
        let right = self.source.sample(1, frame);
        match self.channels {
            ChannelMode::Stereo => (left, right),
            ChannelMode::Mono => {
                let mixed = (left + right) * 0.5;
                (mixed, mixed)
            }
            ChannelMode::Left => (left, left),
            ChannelMode::Right => (right, right),
        }
    }

    pub fn end(&self) -> Frames {
        self.start.saturating_add(self.len)
    }

    /// Shows timeline `start..end` of the clip's audio. The audio stays where it is on the timeline,
    /// so a trimmed part can be shown again later. Volume points keep their place in the audio.
    pub fn trim(&mut self, start: Frames, end: Frames) -> crate::Result<()> {
        let len = end.saturating_sub(start);
        let offset = self.offset.saturating_add(start.saturating_sub(self.start));
        if len <= 0 {
            return Err(crate::Error::Invalid("a clip cannot be trimmed to nothing".into()));
        }
        if end <= 0 {
            return Err(crate::Error::Invalid("a clip must end after 0".into()));
        }
        // Generated sound has no end: the source is made again for exactly the new span.
        let offset = if let Some(g) = &self.source.generated {
            let first = g.first.saturating_add(offset);
            let source = g.generator.make(self.source.sample_rate, g.fitted, first, len)?;
            self.source = std::sync::Arc::new(source);
            0
        } else if offset < 0 || offset.saturating_add(len) > self.source.frames() {
            return Err(crate::Error::Invalid("the trim goes past the end of the audio".into()));
        } else {
            offset
        };
        let shift = start - self.start;
        for point in &mut self.gain {
            point.at = point.at.saturating_sub(shift);
        }
        self.start = start;
        self.offset = offset;
        self.len = len;
        Ok(())
    }

    /// Frames of audio hidden before the clip's start and after its end.
    pub fn hidden(&self) -> (Frames, Frames) {
        let after = self
            .source
            .frames()
            .saturating_sub(self.offset.saturating_add(self.len));
        (self.offset.max(0), after.max(0))
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub gain_db: f32,
    pub muted: bool,
    /// Sorted by start. Two clips may overlap, and the overlap is their crossfade. No clip lies
    /// fully inside another, and no point is covered by three clips.
    pub clips: Vec<Clip>,
    /// Applied to the track's sound from the first lane down, before the track volume.
    pub effects: Vec<EffectLane>,
}

#[derive(Debug, Clone, Default)]
pub struct Project {
    /// 0 until the first clip is added; that clip sets it.
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
    /// When set, the project is a loop of this many frames: audio past it plays again from 0,
    /// crossfaded with what is there. Clips then start in `0..loop_end`.
    pub loop_end: Option<Frames>,
    pub(crate) next_id: u64,
    /// Applied to the mix of all tracks from the first lane down.
    pub mix_effects: Vec<EffectLane>,
    /// Audio whose file now lives elsewhere, because the file it was read from was overwritten. Keyed by the
    /// audio, not the path, so audio read later from the new file at the old path is not affected.
    pub(crate) relocated: Vec<(Arc<Source>, PathBuf)>,
    pub export: crate::ExportTarget,
    /// Named parts of the mix to export as files of their own, shown above the tracks.
    pub region_lanes: Vec<crate::RegionLane>,
    /// See [`Project::sound_id`].
    pub(crate) sound_id: u64,
}

impl Project {
    /// The same for two snapshots only when they sound the same, so a mixdown made from one serves the other.
    /// Edits that only change what is exported keep it.
    pub fn sound_id(&self) -> u64 {
        self.sound_id
    }

    /// Where the audio is stored on disk now.
    pub fn file_of<'a>(&'a self, source: &'a Arc<Source>) -> Option<&'a Path> {
        let moved = self.relocated.iter().find(|(s, _)| Arc::ptr_eq(s, source));
        moved.map(|(_, file)| file.as_path()).or(source.path.as_deref())
    }

    /// Every distinct piece of audio the clips use.
    pub fn sources(&self) -> Vec<&Arc<Source>> {
        let mut found: Vec<&Arc<Source>> = vec![];
        for clip in self.tracks.iter().flat_map(|t| &t.clips) {
            if !found.iter().any(|s| Arc::ptr_eq(s, &clip.source)) {
                found.push(&clip.source);
            }
        }
        found
    }

    pub(crate) fn new_id(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn clip(&self, id: ClipId) -> Option<(&Track, &Clip)> {
        self.tracks
            .iter()
            .find_map(|t| t.clips.iter().find(|c| c.id == id).map(|c| (t, c)))
    }

    /// The length that is played and exported: the loop, or up to the end of the last clip.
    /// With a loop exactly one loop; otherwise up to the end of the last clip or the last effect tail.
    pub fn len(&self) -> Frames {
        self.loop_end.unwrap_or_else(|| self.clips_end().max(self.tails_end()))
    }

    fn tails_end(&self) -> Frames {
        let lanes = self.tracks.iter().flat_map(|t| &t.effects).chain(&self.mix_effects);
        lanes
            .flat_map(|lane| &lane.ranges)
            .filter(|r| r.effect.tail(self.sample_rate) > 0)
            .map(|r| r.end)
            .max()
            .unwrap_or(0)
    }

    /// The lane with this id, under a track or under the mix.
    pub fn lane(&self, id: LaneId) -> Option<&EffectLane> {
        let mut lanes = self.tracks.iter().flat_map(|t| &t.effects).chain(&self.mix_effects);
        lanes.find(|l| l.id == id)
    }

    pub fn region_lane(&self, id: LaneId) -> Option<&crate::RegionLane> {
        self.region_lanes.iter().find(|l| l.id == id)
    }

    /// The region with this id and the lane holding it.
    pub fn region(&self, id: crate::RangeId) -> Option<(&crate::RegionLane, &crate::Region)> {
        self.region_lanes.iter().find_map(|l| Some((l, l.region(id)?)))
    }

    /// End of the last clip. With a loop, this can be past the loop end.
    pub fn clips_end(&self) -> Frames {
        self.tracks
            .iter()
            .flat_map(|t| &t.clips)
            .map(Clip::end)
            .max()
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.iter().all(|t| t.clips.is_empty())
    }
}
