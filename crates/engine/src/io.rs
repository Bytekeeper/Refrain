//! Reading audio files, converting the sample rate, and exporting the mix.

use std::path::Path;

use craft_audio::{AudioBuffer, BitDepth, DecodeOptions, EncodeFormat, EncodeOptions};
use rubato::audioadapter::Adapter;
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

use crate::{Error, Frames, Project, Result, Source, mixdown};

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
        generated: None,
    })
}

/// The same audio at `rate`, converted with an FFT resampler, which keeps everything below the lower
/// of the two Nyquist frequencies. The file the audio came from is not touched.
pub fn resample(source: &Source, rate: u32) -> Result<Source> {
    let failed = |e: &dyn std::fmt::Display| Error::Resample(e.to_string());
    if rate == 0 || source.sample_rate == 0 {
        return Err(Error::Resample("a sample rate of 0 Hz".into()));
    }
    let frames = usize::try_from(source.frames()).unwrap_or(0);
    let channels = source.channels.len();
    if frames == 0 || channels == 0 {
        return Err(Error::EmptySource);
    }
    let converted = if source.sample_rate == rate {
        source
            .channels
            .iter()
            .map(|c| c.iter().take(frames).copied().collect())
            .collect()
    } else {
        let mut resampler = Fft::<f32>::new(
            source.sample_rate as usize,
            rate as usize,
            1024,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| failed(&e))?;
        let input = SequentialSliceOfVecs::new(source.channels.as_slice(), channels, frames).map_err(|e| failed(&e))?;
        let output = resampler.process_all(&input, frames, None).map_err(|e| failed(&e))?;
        (0..channels)
            .map(|c| {
                (0..output.frames())
                    .map(|f| output.read_sample(c, f).unwrap_or(0.0))
                    .collect()
            })
            .collect()
    };
    Ok(Source {
        path: source.path.clone(),
        name: source.name.clone(),
        sample_rate: rate,
        channels: converted,
        generated: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Wav,
    Flac,
    Ogg,
    Mp3,
    M4a,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] = [Self::Wav, Self::Flac, Self::Ogg, Self::Mp3, Self::M4a];

    pub fn name(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Flac => "FLAC",
            Self::Ogg => "Ogg Vorbis",
            Self::Mp3 => "MP3",
            Self::M4a => "M4A (AAC)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Mp3 => "mp3",
            Self::M4a => "m4a",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.extension().eq_ignore_ascii_case(ext))
    }

    /// Lossy formats take a bit rate; the others a bit depth.
    pub fn is_lossy(self) -> bool {
        matches!(self, Self::Ogg | Self::Mp3 | Self::M4a)
    }

    /// The rate a file of this format is written at when `wanted` is asked for. MP3 only has rates up to 48 kHz;
    /// a rate it divides evenly is preferred, so 88.2 kHz becomes 44.1 kHz.
    pub fn rate_for(self, wanted: u32) -> u32 {
        const MP3: [u32; 6] = [48_000, 44_100, 32_000, 24_000, 22_050, 16_000];
        if self != Self::Mp3 || MP3.contains(&wanted) {
            return wanted;
        }
        MP3.into_iter()
            .find(|&r| wanted.is_multiple_of(r))
            .or_else(|| MP3.into_iter().min_by_key(|&r| r.abs_diff(wanted)))
            .unwrap_or(48_000)
    }
}

/// Where and how "export again" writes the mix. Saved with the project.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ExportTarget {
    pub file: Option<std::path::PathBuf>,
    pub settings: ExportSettings,
}

/// How the mix is written. Values may come from a project file, so `export` checks them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExportSettings {
    pub format: ExportFormat,
    /// WAV: 16, 24 or 32 (32 is float). FLAC: 16 or 24.
    pub bits: u16,
    /// For the lossy formats.
    pub kbps: u32,
    /// Left and right mixed into one channel, as game engines want for sounds placed in space.
    pub mono: bool,
    /// `None` is the project rate.
    pub sample_rate: Option<u32>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings {
            format: ExportFormat::Wav,
            bits: 24,
            kbps: 192,
            mono: false,
            sample_rate: None,
        }
    }
}

/// What [`export`] wrote.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Written {
    /// The sample rate of the file.
    pub rate: u32,
    /// Samples over 0 dB. The integer formats cut them off; float WAV keeps them, but game engines cut them.
    pub over: usize,
    /// The loudest sample, as an amplitude (1 is 0 dB), and where it is in project frames.
    pub peak: f32,
    pub peak_at: Frames,
}

/// Writes the whole mix (with the loop on: exactly one loop).
pub fn export(project: &Project, settings: &ExportSettings, path: &Path) -> Result<Written> {
    if project.is_empty() {
        return Err(Error::Invalid("the project has no audio to export".into()));
    }
    let sound = mixdown(project, || true)
        .ok_or_else(|| Error::Invalid("the mix was stopped".into()))?
        .sound;
    write_sound(sound, project.sample_rate, settings, path)
}

/// Writes `sound`, made at `project_rate`. Where the peak is counts from the first frame of `sound`.
pub(crate) fn write_sound(
    [left, right]: [Vec<f32>; 2],
    project_rate: u32,
    settings: &ExportSettings,
    path: &Path,
) -> Result<Written> {
    let format = settings.format;
    let rate = format.rate_for(settings.sample_rate.unwrap_or(project_rate));
    let mut channels = if settings.mono {
        vec![left.iter().zip(&right).map(|(l, r)| (l + r) * 0.5).collect()]
    } else {
        vec![left, right]
    };
    if rate != project_rate {
        let mix = Source {
            path: None,
            name: "mix".into(),
            sample_rate: project_rate,
            channels,
            generated: None,
        };
        channels = resample(&mix, rate)?.channels;
    }
    // Measured on what is written, after the mono mix and the rate change.
    let (mut over, mut peak, mut peak_index) = (0, 0.0f32, 0);
    for channel in &channels {
        for (i, s) in channel.iter().enumerate() {
            over += usize::from(s.abs() > 1.0);
            if s.abs() > peak {
                (peak, peak_index) = (s.abs(), i);
            }
        }
    }
    let peak_at = (peak_index as f64 * f64::from(project_rate) / f64::from(rate.max(1))).round() as Frames;
    let mut options = EncodeOptions::new(match format {
        ExportFormat::Wav => EncodeFormat::Wav,
        ExportFormat::Flac => EncodeFormat::Flac,
        ExportFormat::Ogg => EncodeFormat::Ogg,
        ExportFormat::Mp3 => EncodeFormat::Mp3,
        ExportFormat::M4a => EncodeFormat::Aac,
    });
    if format.is_lossy() {
        options = options.with_bitrate(settings.kbps.saturating_mul(1000));
    } else {
        let depth = match (format, settings.bits) {
            (_, 16) => BitDepth::Int16,
            (_, 24) => BitDepth::Int24,
            (ExportFormat::Wav, 32) => BitDepth::Float32,
            (_, bits) => {
                return Err(Error::Invalid(format!(
                    "{} cannot be written with {bits} bits",
                    format.name()
                )));
            }
        };
        options = options.with_bit_depth(depth).with_dither(depth == BitDepth::Int16);
    }
    let bytes = craft_audio::encode(
        &AudioBuffer {
            sample_rate: rate,
            channels,
        },
        &options,
    )?;
    write_atomic(path, &bytes)?;
    Ok(Written {
        rate,
        over,
        peak,
        peak_at,
    })
}

/// Writes next to the target and renames, so a full disk never leaves a half-written file behind.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_a_tone_and_its_length() {
        let tone = |rate: u32, frames: usize| -> Vec<f32> {
            let step = 1000.0 * std::f64::consts::TAU / f64::from(rate);
            (0..frames).map(|i| (0.5 * (i as f64 * step).sin()) as f32).collect()
        };
        let source = Source {
            path: None,
            name: "tone".into(),
            sample_rate: 48_000,
            channels: vec![tone(48_000, 48_000)],
            generated: None,
        };
        let converted = resample(&source, 44_100).unwrap();
        assert_eq!(converted.sample_rate, 44_100);
        assert!(
            (converted.frames() - 44_100).abs() <= 2,
            "{} frames",
            converted.frames()
        );
        // Away from the ends, which the filter cannot see past, it is the same tone at the new rate.
        let expected = tone(44_100, 44_100);
        let pairs = converted.channels[0].iter().zip(&expected).enumerate();
        for (i, (got, want)) in pairs.skip(1000).take(42_000) {
            let error = (got - want).abs();
            assert!(error < 0.002, "frame {i}: off by {error}");
        }
    }
}
