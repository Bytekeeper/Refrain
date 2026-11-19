//! Audio output: plays the latest mixdown through the default device. The audio thread only copies from it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use refrain_engine::{Frames, Project};

/// What the audio thread reads. The window swaps in each new mixdown as soon as it is made.
struct Shared {
    sound: Arc<[Vec<f32>; 2]>,
    loop_end: Option<Frames>,
    pos: Frames,
    /// How far past `pos` playback is, between 0 and 1: below speed 1 a frame plays for several output frames.
    between: f64,
    playing: bool,
    /// Like a tape: below 1 slower and lower, so a click stays a click and can be found by ear.
    speed: f64,
    /// Where playback was before a seek while playing; it fades out over `blend_left` more output frames.
    leaving: Option<(Frames, f64)>,
    blend_left: u32,
}

/// Output frames over which a seek while playing fades from the old place to the new one: a jump straight
/// from one place in the wave to another is heard as a click. About 5 ms, so the seek still sounds at once.
const BLEND: u32 = 256;

impl Default for Shared {
    fn default() -> Self {
        Shared {
            sound: Arc::default(),
            loop_end: None,
            pos: 0,
            between: 0.0,
            playing: false,
            speed: 1.0,
            leaving: None,
            blend_left: 0,
        }
    }
}

impl Shared {
    fn seek(&mut self, pos: Frames) {
        if self.playing {
            self.leaving = Some((self.pos, self.between));
            self.blend_left = BLEND;
        }
        self.pos = pos.max(0);
        self.between = 0.0;
    }
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
        lock(&self.shared).seek(pos);
    }

    pub fn speed(&self) -> f64 {
        lock(&self.shared).speed
    }

    pub fn set_speed(&self, speed: f64) {
        lock(&self.shared).speed = speed.clamp(0.01, 1.0);
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
    let (sound, loop_end, here, leaving, blend_left, speed) = {
        let s = lock(shared);
        if !s.playing || channels == 0 {
            return;
        }
        (
            s.sound.clone(),
            s.loop_end,
            (s.pos, s.between),
            s.leaving,
            s.blend_left,
            s.speed,
        )
    };
    let [left, right] = &*sound;
    let get = |c: &Vec<f32>, t: Frames| {
        let at = loop_end.map_or(t, |l| t.rem_euclid(l));
        usize::try_from(at).ok().and_then(|i| c.get(i)).copied().unwrap_or(0.0)
    };
    // The k-th output frame played from a place, reading between frames below speed 1.
    let at = |(pos, between): (Frames, f64), k: u32| {
        let t = between + f64::from(k) * speed;
        let (whole, part) = (t.floor(), (t - t.floor()) as f32);
        let t = pos.saturating_add(whole as Frames);
        let read = |c: &Vec<f32>| get(c, t) * (1.0 - part) + get(c, t + 1) * part;
        (read(left), read(right))
    };
    for (frame, k) in out.chunks_mut(channels).zip(0u32..) {
        let (mut l, mut r) = at(here, k);
        if let Some(old) = leaving
            && k < blend_left
        {
            let w = (blend_left - k) as f32 / BLEND as f32;
            let (old_l, old_r) = at(old, k);
            (l, r) = (l + (old_l - l) * w, r + (old_r - r) * w);
        }
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
    let frames = out.len() / channels;
    let advance = |(pos, between): (Frames, f64)| {
        let moved = between + frames as f64 * speed;
        let pos = pos.saturating_add(moved.floor() as Frames);
        (loop_end.map_or(pos, |l| pos.rem_euclid(l)), moved - moved.floor())
    };
    let mut s = lock(shared);
    // A seek from the window during copying wins over advancing.
    if (s.pos, s.between) == here {
        (s.pos, s.between) = advance(here);
        if loop_end.is_none() && s.pos >= left.len() as Frames {
            s.playing = false;
        }
        let frames = u32::try_from(frames).unwrap_or(u32::MAX);
        s.leaving = leaving.filter(|_| blend_left > frames).map(advance);
        s.blend_left = blend_left.saturating_sub(frames);
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
            ..Shared::default()
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

    /// Regression: a seek while playing jumped from one place in the wave to another and was heard as a click.
    #[test]
    fn a_seek_while_playing_does_not_jump() {
        let tone: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.05).sin() * 0.8).collect();
        // The steepest the tone itself goes from one frame to the next.
        let steepest = 0.05 * 0.8;
        let shared = Mutex::new(Shared {
            sound: Arc::new([tone.clone(), tone]),
            playing: true,
            ..Shared::default()
        });
        let mut before = [0.0; 128];
        fill(&mut before, 2, &shared);
        // The tone's peak, where the frames played last are near zero.
        lock(&shared).seek(10_000 + 31);
        // In small pieces, as a device may ask: the blend goes on from one to the next.
        let mut after = [0.0; 1024];
        for piece in after.chunks_mut(64) {
            fill(piece, 2, &shared);
        }
        let heard: Vec<f32> = before.iter().chain(&after).step_by(2).copied().collect();
        let jump = heard.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(jump < 2.0 * steepest, "jump {jump}, the tone alone {steepest}");
    }

    #[test]
    fn half_speed_plays_each_frame_twice_as_long_reading_between_them() {
        let sound: Arc<[Vec<f32>; 2]> = Arc::new([vec![0.0, 0.4, 0.8, 0.4, 0.0], vec![0.0; 5]]);
        let shared = Mutex::new(Shared {
            sound,
            playing: true,
            speed: 0.5,
            ..Shared::default()
        });
        let mut out = [0.0; 3];
        fill(&mut out, 1, &shared);
        assert_eq!(out, [0.0, 0.1, 0.2], "mono: half of the left");
        let s = lock(&shared);
        assert_eq!((s.pos, s.between), (1, 0.5));
        drop(s);
        fill(&mut out, 1, &shared);
        assert_eq!(out, [0.3, 0.4, 0.3]);
    }
}
