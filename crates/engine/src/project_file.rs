//! The project file: JSON that refers to the audio files by path and holds every edit.
//! Opening treats the file as hostile: every number is checked before it reaches the model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::io::write_atomic;
use crate::session::check_db;
use crate::{
    Clip, ClipId, Error, Fade, FadeShape, Frames, GainPoint, Project, Result, Session, Source, Track, TrackId,
};

pub const PROJECT_EXTENSION: &str = "audioedit";
const VERSION: u32 = 1;
/// A project file holds only edits; a file this large is not one.
const MAX_FILE_BYTES: u64 = 64 << 20;

#[derive(Deserialize)]
struct Header {
    version: u32,
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    sample_rate: u32,
    /// Relative to the project file's folder when the audio is inside it, so the folder can move.
    sources: Vec<PathBuf>,
    tracks: Vec<TrackFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loop_end: Option<Frames>,
}

#[derive(Serialize, Deserialize)]
struct TrackFile {
    name: String,
    gain_db: f32,
    muted: bool,
    clips: Vec<ClipFile>,
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
}

pub fn save_project(project: &Project, path: &Path) -> Result<()> {
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
                    let file = clip.source.path.as_deref().ok_or_else(|| {
                        Error::ProjectFile(format!("\"{}\" was not read from a file", clip.source.name))
                    })?;
                    let file = std::path::absolute(file)?;
                    sources.push(file.strip_prefix(dir).map(Path::to_path_buf).unwrap_or(file));
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
            });
        }
        tracks.push(TrackFile {
            name: track.name.clone(),
            gain_db: track.gain_db,
            muted: track.muted,
            clips,
        });
    }
    let file = ProjectFile {
        version: VERSION,
        sample_rate: project.sample_rate,
        sources,
        tracks,
        loop_end: project.loop_end,
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| Error::ProjectFile(e.to_string()))?;
    write_atomic(&path, &bytes)
}

pub fn open_project(path: &Path) -> Result<Session> {
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
        let full = dir.join(stored);
        let source = crate::load_source(&full)
            .map_err(|e| Error::ProjectFile(format!("cannot read {}: {e}", full.display())))?;
        if source.sample_rate != file.sample_rate {
            return Err(Error::SampleRateMismatch {
                project: file.sample_rate,
                found: source.sample_rate,
            });
        }
        sources.push(Arc::new(source));
    }

    let mut project = Project {
        sample_rate: file.sample_rate,
        loop_end: file.loop_end,
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
        });
    }
    Session::open(project)
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
    })
}

/// JSON has no infinity, but a hand-edited file may hold any number.
fn finite_db(db: f32) -> Result<f32> {
    if !db.is_finite() {
        return Err(Error::ProjectFile("a volume is not a finite number".into()));
    }
    check_db(db)
}
