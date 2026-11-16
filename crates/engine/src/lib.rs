//! The editor without a window: project model, commands with undo, mixing, import and export.

mod io;
mod mix;
mod model;
mod project_file;
mod session;

pub use io::{ExportFormat, ExportSettings, export, load_source, resample};
pub use mix::{clip_amp, edge_fades, fade_gain, overlap_after, overlap_before, render, similarity};
pub use model::{ChannelMode, Clip, ClipId, Fade, FadeShape, Frames, GainPoint, Project, Source, Track, TrackId};
pub use project_file::{PROJECT_EXTENSION, open_project, save_project};
pub use session::{Command, Session};

pub const MIN_DB: f32 = -80.0;
pub const MAX_DB: f32 = 24.0;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no track with id {0:?}")]
    NoTrack(TrackId),
    #[error("no clip with id {0:?}")]
    NoClip(ClipId),
    #[error("the file has no audio")]
    EmptySource,
    #[error("cannot convert the sample rate: {0}")]
    Resample(String),
    #[error("{0}")]
    Invalid(String),
    #[error("project file: {0}")]
    ProjectFile(String),
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error(transparent)]
    Audio(#[from] craft_audio::AudioError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
