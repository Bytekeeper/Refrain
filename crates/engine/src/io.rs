use std::path::Path;

use craft_audio::{AudioBuffer, DecodeOptions, EncodeOptions};

use crate::{Error, Project, Result, Source, render};

/// About 50 minutes of 48 kHz stereo in memory. Streaming from disk is a later step.
const MAX_SAMPLES: u64 = 300_000_000;

pub fn load_source(path: &Path) -> Result<Source> {
    let bytes = std::fs::read(path)?;
    let ext = path.extension().and_then(|e| e.to_str());
    let options = DecodeOptions::default().with_max_samples(MAX_SAMPLES);
    let (info, buffer) = craft_audio::decode_with(&bytes, ext, &options)?;
    if buffer.frames() == 0 || info.sample_rate == 0 {
        return Err(Error::EmptySource);
    }
    let name = path
        .file_stem()
        .map_or_else(|| "audio".into(), |s| s.to_string_lossy().into_owned());
    Ok(Source {
        path: Some(path.to_path_buf()),
        name,
        sample_rate: info.sample_rate,
        channels: buffer.channels,
    })
}

/// Writes the whole mix as a 24-bit stereo WAV file.
pub fn export_wav(project: &Project, path: &Path) -> Result<()> {
    if project.is_empty() {
        return Err(Error::Invalid("the project has no audio to export".into()));
    }
    let frames = usize::try_from(project.len()).map_err(|_| Error::Invalid("the project is too long".into()))?;
    let channels = render(project, 0, frames).to_vec();
    let bytes = craft_audio::encode(
        &AudioBuffer {
            sample_rate: project.sample_rate,
            channels,
        },
        &EncodeOptions::default(),
    )?;
    write_atomic(path, &bytes)
}

/// Writes next to the target and renames, so a full disk never leaves a half-written file behind.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}
