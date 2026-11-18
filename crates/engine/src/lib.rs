//! The editor without a window: project model, commands with undo, mixing, effects, generated sound,
//! project files, import and export.

mod effects;
mod generate;
mod io;
mod loudness;
mod mix;
mod model;
mod pitch;
mod project_file;
mod regions;
mod session;

pub use effects::{EQ_BANDS_HZ, Effect, EffectLane, EffectRange, Gain, LaneId, RangeId};
pub use generate::{Generated, Generator, NoiseColor, Wave, color_name, played_hz, wave_name};
pub use io::{ExportFormat, ExportSettings, ExportTarget, Written, change_speed, export, load_source, resample};
pub use mix::{
    Levels, Mixdown, clip_amp, edge_fades, fade_gain, mixdown, overlap_after, overlap_before, render, similarity,
};
pub use model::{
    ChannelMode, Clip, ClipId, Fade, FadeShape, Frames, GainPoint, MAX_SPEED, MIN_SPEED, Project, Source, Sped, Track,
    TrackId,
};
pub use project_file::{PROJECT_EXTENSION, open_project, save_project};
pub use regions::{Region, RegionLane};
pub use session::{Command, ExportPart, Exported, Session};

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
