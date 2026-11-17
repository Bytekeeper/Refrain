//! Audio output: plays the latest mixdown through the default device. The audio thread only copies from it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use refrain_engine::{Frames, Project};

/// What the audio thread reads. The window swaps in each new mixdown as soon as it is made.
#[derive(Default)]
struct Shared {
    sound: Arc<[Vec<f32>; 2]>,
    loop_end: Option<Frames>,
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

    /// Swaps in the latest mixdown, made from `project`. A stream opened at another rate would play it at the
    /// wrong speed and pitch, so after a sample rate change the stream is opened again at the new rate.
    pub fn set_mix(&mut self, project: &Project, sound: Arc<[Vec<f32>; 2]>) -> Result<(), String> {
        let rate = project.sample_rate;
        let stale = self.stream.as_ref().is_some_and(|(_, r)| *r != rate);
        if stale {
            self.stream = None;
        }
        {
            let mut shared = lock(&self.shared);
            shared.sound = sound;
            shared.loop_end = project.loop_end.filter(|l| *l > 0);
        }
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
    let stream = device
        .build_output_stream(
            config.config(),
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| fill(out, channels, &shared),
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| format!("Cannot open the audio device: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("Cannot start the audio device: {e}"))?;
    Ok(stream)
}

/// Runs on the audio thread. Copies without holding the lock so the window never waits on it.
fn fill(out: &mut [f32], channels: usize, shared: &Mutex<Shared>) {
    out.fill(0.0);
    let (sound, loop_end, pos) = {
        let s = lock(shared);
        if !s.playing || channels == 0 {
            return;
        }
        (s.sound.clone(), s.loop_end, s.pos)
    };
    let [left, right] = &*sound;
    for (frame, t) in out.chunks_mut(channels).zip(pos..) {
        let at = loop_end.map_or(t, |l| t.rem_euclid(l));
        let get = |c: &Vec<f32>| usize::try_from(at).ok().and_then(|i| c.get(i)).copied().unwrap_or(0.0);
        let (l, r) = (get(left).clamp(-1.0, 1.0), get(right).clamp(-1.0, 1.0));
        match frame {
            [mono] => *mono = (l + r) * 0.5,
            [a, b, ..] => {
                *a = l;
                *b = r;
            }
            [] => {}
        }
    }
    let frames = Frames::try_from(out.len() / channels).unwrap_or(0);
    let mut s = lock(shared);
    // A seek from the window during copying wins over advancing.
    if s.pos == pos {
        s.pos = pos.saturating_add(frames);
        match loop_end {
            Some(l) => s.pos = s.pos.rem_euclid(l),
            None if s.pos >= left.len() as Frames => s.playing = false,
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_round_the_loop_and_stops_at_the_end_without_one() {
        let sound: Arc<[Vec<f32>; 2]> = Arc::new([vec![0.1, 0.2, 0.3, 0.4], vec![0.5, 0.6, 0.7, 0.8]]);
        let shared = Mutex::new(Shared {
            sound,
            loop_end: Some(4),
            pos: 3,
            playing: true,
        });
        let mut out = [0.0; 6];
        fill(&mut out, 2, &shared);
        assert_eq!(out, [0.4, 0.8, 0.1, 0.5, 0.2, 0.6]);
        assert_eq!(lock(&shared).pos, 2);

        lock(&shared).loop_end = None;
        lock(&shared).pos = 3;
        fill(&mut out, 2, &shared);
        assert_eq!(out, [0.4, 0.8, 0.0, 0.0, 0.0, 0.0]);
        assert!(!lock(&shared).playing);
    }
}
