//! The project file: JSON that refers to the audio files by path, holds generated sound as its settings,
//! and holds every edit.
//! Opening treats the file as hostile: every number is checked before it reaches the model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::effects::default_fade;
use crate::io::write_atomic;
use crate::session::check_db;
use crate::{
    ChannelMode, Clip, ClipId, Effect, EffectLane, EffectRange, Error, ExportTarget, Fade, FadeShape, Frames,
    GainPoint, Generator, LaneId, Project, RangeId, Result, Session, Source, Track, TrackId,
};

pub const PROJECT_EXTENSION: &str = "refrain";
const VERSION: u32 = 1;
/// A project file holds only edits; a file this large is not one.
const MAX_FILE_BYTES: u64 = 64 << 20;

/// An audio file, or the settings of generated sound and which part of it the clips use.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SourceFile {
    /// Relative to the project file's folder when the audio is inside it, so the folder can move.
    File(PathBuf),
    Generated {
        generator: Generator,
        first: Frames,
        frames: Frames,
    },
}

#[derive(Deserialize)]
struct Header {
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    sample_rate: u32,
    sources: Vec<SourceFile>,
    tracks: Vec<TrackFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loop_end: Option<Frames>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mix_effects: Vec<LaneFile>,
    /// Its file is relative to the project file's folder when it lies inside it. Missing in files from before
    /// the export target was part of the project; the window kept it in `view` then.
    #[serde(default)]
    export: Option<ExportTarget>,
    /// Settings of the program that edits the project, such as zoom. Stored as they come; read only for an
    /// export target from older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct TrackFile {
    name: String,
    gain_db: f32,
    muted: bool,
    clips: Vec<ClipFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    effects: Vec<LaneFile>,
}

/// What the effect measures from the audio is not stored: every mixdown measures it.
#[derive(Serialize, Deserialize)]
struct LaneFile {
    effect: Effect,
    enabled: bool,
    ranges: Vec<RangeFile>,
}

#[derive(Serialize, Deserialize)]
struct RangeFile {
    start: Frames,
    end: Frames,
    /// Missing in files from before ranges had their own settings: then the lane's.
    #[serde(default)]
    effect: Option<Effect>,
    /// Missing in files from before ranges had fades to set: then the fade every range had.
    #[serde(default)]
    fade_in: Option<Frames>,
    #[serde(default)]
    fade_out: Option<Frames>,
}

fn lanes_to_file(lanes: &[EffectLane]) -> Vec<LaneFile> {
    lanes
        .iter()
        .map(|lane| LaneFile {
            effect: lane.effect,
            enabled: lane.enabled,
            ranges: lane
                .ranges
                .iter()
                .map(|r| RangeFile {
                    start: r.start,
                    end: r.end,
                    effect: Some(r.effect),
                    fade_in: Some(r.fade_in),
                    fade_out: Some(r.fade_out),
                })
                .collect(),
        })
        .collect()
}

fn lanes_from_file(project: &mut Project, lanes: Vec<LaneFile>) -> Result<Vec<EffectLane>> {
    let mut out = vec![];
    for file in lanes {
        let mut lane = EffectLane::new(LaneId(project.new_id()), file.effect)?;
        lane.enabled = file.enabled;
        for r in file.ranges {
            lane.ranges.push(EffectRange {
                id: RangeId(project.new_id()),
                start: r.start,
                end: r.end,
                effect: r.effect.unwrap_or(file.effect),
                fade_in: r.fade_in.unwrap_or(default_fade(project.sample_rate)),
                fade_out: r.fade_out.unwrap_or(default_fade(project.sample_rate)),
            });
        }
        out.push(lane);
    }
    Ok(out)
}

#[derive(Serialize, Deserialize)]
struct ClipFile {
    /// Index into `sources`.
    source: usize,
    start: Frames,
    offset: Frames,
    len: Frames,
    fade_in: Fade,
    fade_out: Fade,
    crossfade_shape: FadeShape,
    gain: Vec<GainPoint>,
    #[serde(default)]
    channels: ChannelMode,
}

/// `view` is stored with the project and handed back by [`open_project`] unchanged.
pub fn save_project(project: &Project, path: &Path, view: Option<&serde_json::Value>) -> Result<()> {
    let path = std::path::absolute(path)?;
    let dir = path.parent().unwrap_or(Path::new("/"));
    let mut index: HashMap<usize, usize> = HashMap::new();
    let mut sources = vec![];
    let mut tracks = vec![];
    for track in &project.tracks {
        let mut clips = vec![];
        for clip in &track.clips {
            let key = Arc::as_ptr(&clip.source) as usize;
            let source = match index.get(&key) {
                Some(&i) => i,
                None => {
                    let stored = match &clip.source.generated {
                        Some(g) => SourceFile::Generated {
                            generator: g.generator,
                            first: g.first,
                            frames: clip.source.frames(),
                        },
                        None => {
                            let file = project.file_of(&clip.source).ok_or_else(|| {
                                Error::ProjectFile(format!("\"{}\" was not read from a file", clip.source.name))
                            })?;
                            let file = std::path::absolute(file)?;
                            SourceFile::File(file.strip_prefix(dir).map(Path::to_path_buf).unwrap_or(file))
                        }
                    };
                    sources.push(stored);
                    index.insert(key, sources.len() - 1);
                    sources.len() - 1
                }
            };
            clips.push(ClipFile {
                source,
                start: clip.start,
                offset: clip.offset,
                len: clip.len,
                fade_in: clip.fade_in,
                fade_out: clip.fade_out,
                crossfade_shape: clip.crossfade_shape,
                gain: clip.gain.clone(),
                channels: clip.channels,
            });
        }
        tracks.push(TrackFile {
            name: track.name.clone(),
            gain_db: track.gain_db,
            muted: track.muted,
            clips,
            effects: lanes_to_file(&track.effects),
        });
    }
    let file = ProjectFile {
        version: VERSION,
        sample_rate: project.sample_rate,
        sources,
        tracks,
        loop_end: project.loop_end,
        mix_effects: lanes_to_file(&project.mix_effects),
        export: Some(ExportTarget {
            file: project.export.file.as_ref().map(|f| {
                let f = std::path::absolute(f).unwrap_or_else(|_| f.clone());
                f.strip_prefix(dir).map(Path::to_path_buf).unwrap_or(f)
            }),
            settings: project.export.settings.clone(),
        }),
        view: view.cloned(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| Error::ProjectFile(e.to_string()))?;
    write_atomic(&path, &bytes)
}

/// The project and the `view` it was saved with.
pub fn open_project(path: &Path) -> Result<(Session, Option<serde_json::Value>)> {
    if std::fs::metadata(path)?.len() > MAX_FILE_BYTES {
        return Err(Error::ProjectFile("the file is too large to be a project".into()));
    }
    let bytes = std::fs::read(path)?;
    let not_a_project = |e: serde_json::Error| Error::ProjectFile(format!("not a readable project: {e}"));
    let header: Header = serde_json::from_slice(&bytes).map_err(not_a_project)?;
    if header.version != VERSION {
        return Err(Error::ProjectFile(format!(
            "version {} is not supported; this editor reads version {VERSION}",
            header.version
        )));
    }
    let file: ProjectFile = serde_json::from_slice(&bytes).map_err(not_a_project)?;

    let dir = path.parent().unwrap_or(Path::new("."));
    let mut sources = vec![];
    for stored in &file.sources {
        let stored = match stored {
            SourceFile::File(path) => path,
            &SourceFile::Generated {
                generator,
                first,
                frames,
            } => {
                let made = generator.make(file.sample_rate, file.loop_end, first, frames);
                let made = made.map_err(|e| Error::ProjectFile(format!("generated sound: {e}")))?;
                sources.push(Arc::new(made));
                continue;
            }
        };
        let full = dir.join(stored);
        let source = crate::load_source(&full)
            .map_err(|e| Error::ProjectFile(format!("cannot read {}: {e}", full.display())))?;
        let source = if source.sample_rate == file.sample_rate {
            source
        } else {
            crate::resample(&source, file.sample_rate)?
        };
        sources.push(Arc::new(source));
    }

    let mut project = Project {
        sample_rate: file.sample_rate,
        loop_end: file.loop_end,
        export: export_from_file(file.export, file.view.as_ref(), dir),
        ..Project::default()
    };
    for track in file.tracks {
        let id = TrackId(project.new_id());
        let mut clips = vec![];
        for clip in track.clips {
            let id = ClipId(project.new_id());
            clips.push(clip_from_file(id, clip, &sources)?);
        }
        let gain_db = finite_db(track.gain_db)?;
        project.tracks.push(Track {
            id,
            name: track.name,
            gain_db,
            muted: track.muted,
            clips,
            effects: vec![],
        });
        let effects = lanes_from_file(&mut project, track.effects)?;
        if let Some(t) = project.tracks.last_mut() {
            t.effects = effects;
        }
    }
    project.mix_effects = lanes_from_file(&mut project, file.mix_effects)?;
    Ok((Session::open(project)?, file.view))
}

fn clip_from_file(id: ClipId, c: ClipFile, sources: &[Arc<Source>]) -> Result<Clip> {
    let bad = |what: &str| Error::ProjectFile(what.into());
    let source = sources
        .get(c.source)
        .ok_or_else(|| bad("a clip refers to an audio file that is not listed"))?;
    if c.len <= 0 {
        return Err(bad("a clip has no length"));
    }
    let source_end = c.offset.checked_add(c.len);
    if c.offset < 0 || source_end.is_none_or(|end| end > source.frames()) {
        return Err(bad("a clip lies outside its audio file"));
    }
    let Some(end) = c.start.checked_add(c.len) else {
        return Err(bad("a clip position is too large"));
    };
    if end <= 0 {
        return Err(bad("a clip ends before 0"));
    }
    let fades = c.fade_in.len.checked_add(c.fade_out.len);
    if c.fade_in.len < 0 || c.fade_out.len < 0 || fades.is_none_or(|f| f > c.len) {
        return Err(bad("a clip has fades longer than itself"));
    }
    let mut gain = c.gain;
    for point in &mut gain {
        point.db = finite_db(point.db)?;
    }
    gain.sort_by_key(|p| p.at);
    Ok(Clip {
        id,
        source: source.clone(),
        start: c.start,
        offset: c.offset,
        len: c.len,
        fade_in: c.fade_in,
        fade_out: c.fade_out,
        crossfade_shape: c.crossfade_shape,
        gain,
        channels: c.channels,
    })
}

/// The export target with its file found from the project file's folder.
fn export_from_file(export: Option<ExportTarget>, view: Option<&serde_json::Value>, dir: &Path) -> ExportTarget {
    let older = || serde_json::from_value(view?.get("export")?.clone()).ok();
    let mut export: ExportTarget = export.or_else(older).unwrap_or_default();
    if let Some(file) = &mut export.file
        && file.is_relative()
    {
        *file = dir.join(&*file);
    }
    export
}

/// JSON has no infinity, but a hand-edited file may hold any number.
fn finite_db(db: f32) -> Result<f32> {
    if !db.is_finite() {
        return Err(Error::ProjectFile("a volume is not a finite number".into()));
    }
    check_db(db)
}
