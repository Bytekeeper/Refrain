use std::path::Path;

use craft_audio::{AudioBuffer, DecodeOptions, EncodeOptions};
use rubato::audioadapter::Adapter;
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

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
    })
}

/// Writes the whole mix as a 24-bit stereo WAV file.
/// With `mono`, left and right are mixed into one channel, as game engines want for sounds placed in space.
pub fn export_wav(project: &Project, path: &Path, mono: bool) -> Result<()> {
    if project.is_empty() {
        return Err(Error::Invalid("the project has no audio to export".into()));
    }
    let frames = usize::try_from(project.len()).map_err(|_| Error::Invalid("the project is too long".into()))?;
    let [left, right] = render(project, 0, frames);
    let channels = if mono {
        vec![left.iter().zip(&right).map(|(l, r)| (l + r) * 0.5).collect()]
    } else {
        vec![left, right]
    };
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
