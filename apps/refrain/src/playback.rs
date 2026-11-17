use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use refrain_engine::{Frames, Project, Renderer};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// What the audio thread reads. The window swaps in each new project snapshot, so edits are heard at once.
#[derive(Default)]
struct Shared {
    project: Arc<Project>,
    pos: Frames,
    playing: bool,
}

#[derive(Default)]
pub struct Player {
    shared: Arc<Mutex<Shared>>,
    /// The open output stream and the sample rate it was opened with.
    stream: Option<(cpal::Stream, u32)>,
}

impl Player {
    pub fn position(&self) -> Frames {
        lock(&self.shared).pos
    }

    pub fn is_playing(&self) -> bool {
        lock(&self.shared).playing
    }

    pub fn seek(&self, pos: Frames) {
        lock(&self.shared).pos = pos.max(0);
    }

    pub fn stop(&self) {
        lock(&self.shared).playing = false;
    }

    /// Swaps in the latest project. A stream opened at another rate would play it at the wrong speed and
    /// pitch, so after a sample rate change the stream is opened again at the new rate.
    pub fn set_project(&mut self, project: &Arc<Project>) -> Result<(), String> {
        let rate = project.sample_rate;
        let stale = self.stream.as_ref().is_some_and(|(_, r)| *r != rate);
        if stale {
            self.stream = None;
        }
        lock(&self.shared).project = project.clone();
        if stale && self.is_playing() {
            match open(rate, self.shared.clone()) {
                Ok(stream) => self.stream = Some((stream, rate)),
                Err(e) => {
                    self.stop();
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn play(&mut self, project: &Arc<Project>) -> Result<(), String> {
        if project.is_empty() {
            return Err("There is nothing to play.".into());
        }
        let rate = project.sample_rate;
        if self.stream.as_ref().map(|(_, r)| *r) != Some(rate) {
            self.stream = None;
            self.stream = Some((open(rate, self.shared.clone())?, rate));
        }
        let mut shared = lock(&self.shared);
        shared.project = project.clone();
        if shared.pos >= project.len() {
            shared.pos = 0;
        }
        shared.playing = true;
        Ok(())
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open(rate: u32, shared: Arc<Mutex<Shared>>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("No audio output device found.")?;
    let mut configs: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| format!("Cannot read the audio device settings: {e}"))?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .filter_map(|c| c.try_with_sample_rate(rate))
        .collect();
    configs.sort_by_key(|c| c.channels() != 2);
    let config = configs
        .into_iter()
        .next()
        .ok_or_else(|| format!("The audio device cannot play {rate} Hz; resampling is not supported yet."))?;
    let channels = usize::from(config.channels());
    let mut renderer = Renderer::default();
    let stream = device
        .build_output_stream(
            config.config(),
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| fill(out, channels, &shared, &mut renderer),
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| format!("Cannot open the audio device: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Cannot start the audio device: {e}"))?;
    Ok(stream)
}

/// Runs on the audio thread. Renders without holding the lock so the window never waits on it.
fn fill(out: &mut [f32], channels: usize, shared: &Mutex<Shared>, renderer: &mut Renderer) {
    out.fill(0.0);
    let (project, pos) = {
        let s = lock(shared);
        if !s.playing || channels == 0 {
            return;
        }
        (s.project.clone(), s.pos)
    };
    let frames = out.len() / channels;
    let [left, right] = renderer.render(&project, pos, frames);
    for ((frame, l), r) in out.chunks_mut(channels).zip(&left).zip(&right) {
        let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
        match frame {
            [mono] => *mono = (l + r) * 0.5,
            [a, b, ..] => {
                *a = l;
                *b = r;
            }
            [] => {}
        }
    }
    let mut s = lock(shared);
    // A seek from the window during rendering wins over advancing.
    if s.pos == pos {
        s.pos = pos.saturating_add(Frames::try_from(frames).unwrap_or(0));
        match project.loop_end {
            Some(l) if l > 0 => s.pos = s.pos.rem_euclid(l),
            _ if s.pos >= project.len() => s.playing = false,
            _ => {}
        }
    }
}
