//! Effects in lanes: each lane is one effect applied in its time ranges, lanes in order from the top.

use std::collections::HashMap;
use std::sync::Arc;

use crate::loudness::{from_slices, slices};
use crate::mix::{db_to_amp, mix_upto, render_track};
use crate::{Clip, Error, Frames, Project, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaneId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RangeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Sets the gain of each range so its loudest sample reaches `peak_db`.
    Normalize { peak_db: f32 },
    /// Sets the gain of each range so it sounds as loud as `lufs`, measured after ITU-R BS.1770, but never
    /// further than its loudest sample reaching `peak_db`.
    Loudness { lufs: f32, peak_db: f32 },
    /// Lets through what is above `hz`: removes rumble and hum. `steep` falls 24 dB per octave instead of 12.
    HighPass { hz: f32, steep: bool },
    /// Lets through what is below `hz`: removes hiss and harshness.
    LowPass { hz: f32, steep: bool },
    /// Repeats the sound every `delay_ms`; the first repeat at `level_db`, each next one `fade_db` quieter.
    /// `reversed`: the repeats come before the sound instead, getting louder towards it.
    Echo {
        delay_ms: f32,
        fade_db: f32,
        level_db: f32,
        #[serde(default)]
        reversed: bool,
    },
    /// The sound of a room that dies away to -60 dB in `seconds`, added at `level_db`. `reversed`: the room
    /// comes before the sound instead and swells into it.
    Reverb {
        seconds: f32,
        level_db: f32,
        #[serde(default)]
        reversed: bool,
    },
}

impl Effect {
    /// One of each kind with its usual settings, in menu order.
    pub fn defaults() -> Vec<Effect> {
        vec![
            Effect::Normalize { peak_db: -1.0 },
            Effect::Loudness {
                lufs: -16.0,
                peak_db: -1.0,
            },
            Effect::HighPass { hz: 80.0, steep: false },
            Effect::LowPass {
                hz: 8000.0,
                steep: false,
            },
            Effect::Echo {
                delay_ms: 300.0,
                fade_db: 6.0,
                level_db: -6.0,
                reversed: false,
            },
            Effect::Reverb {
                seconds: 1.5,
                level_db: -12.0,
                reversed: false,
            },
        ]
    }

    /// Whether the effect's tail comes before the range instead of after it.
    pub fn reversed(&self) -> bool {
        matches!(
            self,
            Effect::Echo { reversed: true, .. } | Effect::Reverb { reversed: true, .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Effect::Normalize { .. } => "Normalize",
            Effect::Loudness { .. } => "Loudness",
            Effect::HighPass { .. } => "High-pass",
            Effect::LowPass { .. } => "Low-pass",
            Effect::Echo { .. } => "Echo",
            Effect::Reverb { .. } => "Reverb",
        }
    }

    /// How long after a range the effect still sounds: until it is 60 dB below the sound that went in.
    pub fn tail(&self, rate: u32) -> Frames {
        match *self {
            Effect::Normalize { .. } | Effect::Loudness { .. } | Effect::HighPass { .. } | Effect::LowPass { .. } => 0,
            Effect::Echo {
                delay_ms,
                fade_db,
                level_db,
                ..
            } => {
                let repeats = ((60.0 + level_db) / fade_db.max(0.1)).ceil().max(0.0) as Frames + 1;
                ms_frames(delay_ms, rate).saturating_mul(repeats)
            }
            Effect::Reverb { seconds, .. } => {
                ms_frames(seconds * 1000.0, rate).saturating_add(ms_frames(COMB_MS[7] + SPREAD_MS, rate))
            }
        }
    }

    /// The settings that make the tail about `frames` long, as near as the limits allow. An echo keeps its
    /// delay and first volume and changes how many repeats are heard, so its tail is whole repeats long.
    pub fn with_tail(self, frames: Frames, rate: u32) -> Effect {
        match self {
            Effect::Reverb { level_db, reversed, .. } => {
                let extra = ms_frames(COMB_MS[7] + SPREAD_MS, rate);
                let seconds = (frames - extra) as f32 / rate.max(1) as f32;
                Effect::Reverb {
                    seconds: (seconds * 100.0).round().clamp(10.0, 1000.0) / 100.0,
                    level_db,
                    reversed,
                }
            }
            Effect::Echo {
                delay_ms,
                level_db,
                reversed,
                ..
            } => {
                let delay = ms_frames(delay_ms, rate);
                let span = 60.0 + level_db;
                // tail() counts ceil(span / fade) + 1 repeats; fade from 40 down to 3 dB.
                let fewest = (span / 40.0).ceil().max(1.0) as Frames + 1;
                let most = (span / 3.0).floor() as Frames + 1;
                let repeats = ((frames + delay / 2) / delay.max(1)).clamp(fewest, most.max(fewest));
                // A hair above the exact value, so rounding cannot add a repeat.
                let fade_db = (span / (repeats - 1) as f32 * 1.0001).clamp(3.0, 40.0);
                Effect::Echo {
                    delay_ms,
                    fade_db,
                    level_db,
                    reversed,
                }
            }
            _ => self,
        }
    }

    /// How much audio before a frame the effect needs to compute it. Rendering starts this much earlier, so a
    /// render from the middle sounds like one from the start. For a filter its memory of the past fades to
    /// -80 dB in this time. A reversed effect is computed in advance and needs none.
    pub(crate) fn warmup(&self, rate: u32) -> Frames {
        if self.reversed() {
            return 0;
        }
        if let Effect::Echo { .. } | Effect::Reverb { .. } = self {
            return self.tail(rate);
        }
        let Some((hz, stages)) = self.filter() else { return 0 };
        let w0 = std::f64::consts::TAU * f64::from(cutoff(hz, rate)) / f64::from(rate.max(1));
        // The slowest stage decays by exp(-w0 / (2 q)) per frame.
        let q = stages.iter().copied().fold(0.0f64, f64::max);
        let frames = 9.21 * 2.0 * q / w0.max(1e-9);
        (frames.ceil() as Frames)
            .saturating_mul(stages.len() as Frames)
            .min(Frames::from(rate) * 2)
    }

    /// Cutoff and the Q of each stage, for the filters.
    fn filter(&self) -> Option<(f32, &'static [f64])> {
        // Butterworth: flat until the cutoff; the steep one is two stages.
        const GENTLE: &[f64] = &[std::f64::consts::FRAC_1_SQRT_2];
        const STEEP: &[f64] = &[0.541_196_1, 1.306_563];
        match *self {
            Effect::HighPass { hz, steep } | Effect::LowPass { hz, steep } => {
                Some((hz, if steep { STEEP } else { GENTLE }))
            }
            _ => None,
        }
    }

    fn check(self) -> Result<Effect> {
        let within = |v: f32, r: std::ops::RangeInclusive<f32>| v.is_finite() && r.contains(&v);
        let problem = match self {
            Effect::Normalize { peak_db } if !within(peak_db, -60.0..=0.0) => {
                "normalize goes to a peak between -60 and 0 dB"
            }
            Effect::Loudness { lufs, peak_db } if !within(lufs, -60.0..=0.0) || !within(peak_db, -60.0..=0.0) => {
                "the loudness and the peak limit must be between -60 and 0"
            }
            Effect::HighPass { hz, .. } | Effect::LowPass { hz, .. } if !within(hz, 10.0..=40_000.0) => {
                "a filter's frequency must be between 10 Hz and 40 kHz"
            }
            Effect::Echo { delay_ms, .. } if !within(delay_ms, 10.0..=2000.0) => {
                "an echo's delay must be between 10 ms and 2 s"
            }
            Effect::Echo { fade_db, .. } if !within(fade_db, 3.0..=40.0) => {
                "each echo must be between 3 and 40 dB quieter than the one before"
            }
            Effect::Reverb { seconds, .. } if !within(seconds, 0.1..=10.0) => {
                "a reverb must last between 0.1 and 10 seconds"
            }
            Effect::Echo { level_db, .. } | Effect::Reverb { level_db, .. } if !within(level_db, -40.0..=0.0) => {
                "the volume of an echo or reverb must be between -40 and 0 dB"
            }
            _ => return Ok(self),
        };
        Err(Error::Invalid(problem.into()))
    }
}

fn ms_frames(ms: f32, rate: u32) -> Frames {
    ((f64::from(ms) * f64::from(rate) / 1000.0).round() as Frames).max(1)
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectRange {
    pub id: RangeId,
    pub start: Frames,
    pub end: Frames,
    /// Of the lane's kind.
    pub effect: Effect,
    /// Measured from the audio under the range whenever the project changes; for normalize and loudness the gain.
    pub gain: f32,
    /// Measured with the gain: the peak limit kept a loudness range quieter than its target.
    pub limited: bool,
    /// How long the effect takes to come in after the start and to go before the end, so it does not click.
    pub fade_in: Frames,
    pub fade_out: Frames,
    /// Measured with the gain for a reversed echo or reverb: its whole sound, which ends at the range end.
    pub reversed: Option<Reversed>,
    /// What normalize or loudness measured, kept so a change of their settings needs no new measuring.
    pub(crate) heard: Option<Heard>,
}

/// The sound of a reversed effect. It needs all of the range before its first frame, so it cannot be computed
/// piece by piece while playing.
#[derive(Clone, PartialEq)]
pub struct Reversed {
    /// What the sound was computed from: the effect, the rate and the sound under the range.
    key: u64,
    made_for: (Frames, Frames, Frames, Frames, Effect),
    sound: Arc<[Vec<f32>; 2]>,
}

impl std::fmt::Debug for Reversed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [left, _] = &*self.sound;
        write!(f, "Reversed({} frames)", left.len())
    }
}

/// The fade a new range starts with: short enough not to be heard as a fade, long enough not to click.
pub(crate) fn default_fade(rate: u32) -> Frames {
    Frames::from(rate) / 100
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectLane {
    pub id: LaneId,
    /// The kind of effect, and the settings a new range starts with: those last set in this lane.
    pub effect: Effect,
    pub enabled: bool,
    /// Sorted by start, not overlapping; with a loop each starts in `0..loop_end` and may run past it.
    pub ranges: Vec<EffectRange>,
}

impl EffectLane {
    pub fn new(id: LaneId, effect: Effect) -> Result<EffectLane> {
        Ok(EffectLane {
            id,
            effect: effect.check()?,
            enabled: true,
            ranges: vec![],
        })
    }

    pub fn range(&self, id: RangeId) -> Option<&EffectRange> {
        self.ranges.iter().find(|r| r.id == id)
    }

    /// The same settings for the lane and all its ranges.
    pub fn set_effect(&mut self, effect: Effect) -> Result<()> {
        self.set_range_effect(None, effect)?;
        for range in &mut self.ranges {
            range.effect = self.effect;
        }
        Ok(())
    }

    /// New settings for one range; later ranges start with them too.
    pub fn set_range_effect(&mut self, range: Option<RangeId>, effect: Effect) -> Result<()> {
        if std::mem::discriminant(&effect) != std::mem::discriminant(&self.effect) {
            return Err(Error::Invalid("a lane holds one kind of effect".into()));
        }
        self.effect = effect.check()?;
        if let Some(id) = range {
            let range = self
                .ranges
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| Error::Invalid("no such effect range".into()))?;
            range.effect = self.effect;
        }
        Ok(())
    }
}

/// The fades between the plain and the processed sound at both edges of a range, so it never clicks.
/// Sorts a lane's ranges and refuses ranges that are empty, overlap, or break the loop rules.
pub(crate) fn tidy_lane(lane: &mut EffectLane, loop_end: Option<Frames>) -> Result<()> {
    lane.ranges.sort_by_key(|r| r.start);
    for range in &lane.ranges {
        if std::mem::discriminant(&range.effect) != std::mem::discriminant(&lane.effect) {
            return Err(Error::Invalid("a lane holds one kind of effect".into()));
        }
        range.effect.check()?;
    }
    if lane.ranges.iter().any(|r| r.end <= r.start) {
        return Err(Error::Invalid("an effect range must end after it starts".into()));
    }
    if lane.ranges.iter().any(|r| r.fade_in < 0 || r.fade_out < 0) {
        return Err(Error::Invalid("a fade cannot be shorter than nothing".into()));
    }
    // A range made shorter than its fades shortens both alike.
    for r in &mut lane.ranges {
        let (len, both) = (r.end - r.start, r.fade_in.saturating_add(r.fade_out));
        if both > len {
            r.fade_in = (r.fade_in as f64 * len as f64 / both as f64).floor() as Frames;
            r.fade_out = len - r.fade_in;
        }
    }
    if let Some(l) = loop_end {
        if lane
            .ranges
            .iter()
            .any(|r| r.start < 0 || r.start >= l || r.end - r.start > l)
        {
            return Err(Error::Invalid(
                "with the loop on, an effect range must start before the loop end and be at most one loop long".into(),
            ));
        }
        if let (Some(first), Some(last)) = (lane.ranges.first(), lane.ranges.last())
            && lane.ranges.len() > 1
            && last.end - l > first.start
        {
            return Err(Error::Invalid(
                "effect ranges in one lane cannot overlap; use another lane".into(),
            ));
        }
    }
    if lane.ranges.windows(2).any(|w| matches!(w, [a, b] if a.end > b.start)) {
        return Err(Error::Invalid(
            "effect ranges in one lane cannot overlap; use another lane".into(),
        ));
    }
    Ok(())
}

/// What each effect remembers of the sound before the current piece, by range. Playback keeps it from one
/// piece to the next, so a reverb does not have to hear seconds of the past again for every piece.
#[derive(Default)]
pub struct Memory {
    ranges: HashMap<RangeId, (Effect, u32, State)>,
}

impl Memory {
    /// The memory of this range; empty when the range is new or its settings changed.
    fn of(&mut self, range: &EffectRange, rate: u32) -> &mut State {
        let fresh = || (range.effect, rate, State::new(range.effect, rate));
        let entry = self.ranges.entry(range.id).or_insert_with(fresh);
        if entry.0 != range.effect || entry.1 != rate {
            *entry = fresh();
        }
        &mut entry.2
    }

    pub(crate) fn forget_removed(&mut self, project: &Project) {
        let lanes = project
            .tracks
            .iter()
            .flat_map(|t| &t.effects)
            .chain(&project.mix_effects);
        let kept: Vec<RangeId> = lanes
            .filter(|l| l.enabled)
            .flat_map(|l| l.ranges.iter().map(|r| r.id))
            .collect();
        self.ranges.retain(|id, _| kept.contains(id));
    }
}

enum State {
    Nothing,
    /// The last two inputs and outputs of each stage, per channel.
    Filter(Vec<[[f64; 4]; 2]>),
    Echo {
        lines: [Vec<f32>; 2],
        at: usize,
    },
    Reverb(Box<Reverb>),
}

impl State {
    fn new(effect: Effect, rate: u32) -> State {
        match effect {
            Effect::Normalize { .. } | Effect::Loudness { .. } => State::Nothing,
            Effect::HighPass { .. } | Effect::LowPass { .. } => {
                State::Filter(vec![[[0.0; 4]; 2]; effect.filter().map_or(0, |(_, s)| s.len())])
            }
            Effect::Echo { delay_ms, .. } => {
                let len = usize::try_from(ms_frames(delay_ms, rate)).unwrap_or(1);
                State::Echo {
                    lines: [vec![0.0; len], vec![0.0; len]],
                    at: 0,
                }
            }
            Effect::Reverb { seconds, .. } => State::Reverb(Box::new(Reverb::new(seconds, rate))),
        }
    }
}

/// Applies the lane to `out`, which holds timeline frames from `start`. Inside its ranges the processed sound
/// replaces the plain one, with short fades between them at the edges. Echo and reverb add to the plain sound
/// instead and keep sounding after the range. Every range works on the sound that came into the lane.
pub(crate) fn apply_lane(
    lane: &EffectLane,
    project: &Project,
    out: &mut [Vec<f32>; 2],
    start: Frames,
    memory: &mut Memory,
) {
    if !lane.enabled {
        return;
    }
    let rate = project.sample_rate;
    let loop_end = project.loop_end.filter(|l| *l > 0);
    let end = start.saturating_add(out[0].len() as Frames);
    let dry = out.clone();
    for range in &lane.ranges {
        if range.effect.reversed() {
            if let Some(Reversed { sound, .. }) = &range.reversed {
                add_reversed(range.end, sound, loop_end, start, out);
            }
            continue;
        }
        // A filter needs its warm-up before the range; echo and reverb sound until their tail ends.
        let before = if range.effect.filter().is_some() {
            range.effect.warmup(rate)
        } else {
            0
        };
        let near = loop_end.is_some()
            || (range.start.saturating_sub(before) < end && range.end.saturating_add(range.effect.tail(rate)) > start);
        if !near {
            // Silent by now: a later piece starts it again from silence, which is the same.
            memory.ranges.remove(&range.id);
            continue;
        }
        let weights = range_weights(range, loop_end, start, dry[0].len());
        apply_range(range, rate, &dry, &weights, out, memory.of(range, rate));
    }
}

/// Applies the lane to `sound`: the whole project from frame 0, with a loop one loop. Each range works out
/// what it changes over its span (the range with its tail) from the sound that came into the lane; then all
/// changes are added, so a lane costs what its ranges cover. With a loop a change past the loop end goes on at
/// the start, as in a loop that has always been playing.
pub(crate) fn lane_into(lane: &EffectLane, project: &Project, sound: &mut [Vec<f32>; 2]) {
    if !lane.enabled {
        return;
    }
    let changes: Vec<(Frames, [Vec<f32>; 2])> = lane.ranges.iter().map(|r| range_change(r, project, sound)).collect();
    let loop_end = project.loop_end.filter(|l| *l > 0);
    for (from, change) in changes {
        for (channel, change) in sound.iter_mut().zip(&change) {
            for (t, d) in (from..).zip(change) {
                let at = loop_end.map_or(t, |l| t.rem_euclid(l));
                if let Some(s) = usize::try_from(at).ok().and_then(|i| channel.get_mut(i)) {
                    *s += d;
                }
            }
        }
    }
}

/// What one range adds to `sound`, from the frame returned on.
fn range_change(range: &EffectRange, project: &Project, sound: &[Vec<f32>; 2]) -> (Frames, [Vec<f32>; 2]) {
    let rate = project.sample_rate;
    let loop_end = project.loop_end.filter(|l| *l > 0);
    let read = |from: Frames, n: usize| {
        sound.clone().map(|c| {
            (from..)
                .take(n)
                .map(|t| {
                    let at = loop_end.map_or(t, |l| t.rem_euclid(l));
                    usize::try_from(at).ok().and_then(|i| c.get(i)).copied().unwrap_or(0.0)
                })
                .collect::<Vec<f32>>()
        })
    };
    // A range round the whole loop has no edges: its end runs straight into its start.
    let whole_loop = loop_end.is_some_and(|l| range.end - range.start >= l);
    let (fade_in, fade_out) = (range.fade_in.max(1) as f32, range.fade_out.max(1) as f32);
    let weight = |t: Frames| {
        let (into, left) = ((t - range.start) as f32, (range.end - t) as f32);
        match (t >= range.start && t < range.end, whole_loop) {
            (false, _) => 0.0,
            (true, true) => 1.0,
            (true, false) => ((into + 1.0) / fade_in).min(left / fade_out).min(1.0),
        }
    };
    let len = usize::try_from(range.end - range.start).unwrap_or(0);
    let tail = usize::try_from(range.effect.tail(rate)).unwrap_or(0);
    let state = &mut State::new(range.effect, rate);
    if range.effect.reversed() {
        // Backwards through the range, so the tail comes before it.
        let mut dry = read(range.start, len);
        let mut weights: Vec<f32> = (range.start..range.end).map(weight).collect();
        weights.reverse();
        weights.resize(len + tail, 0.0);
        for channel in &mut dry {
            channel.reverse();
            channel.resize(len + tail, 0.0);
        }
        let mut change = [vec![0.0; len + tail], vec![0.0; len + tail]];
        apply_range(range, rate, &dry, &weights, &mut change, state);
        change.iter_mut().for_each(|c| c.reverse());
        return (range.start - tail as Frames, change);
    }
    let before = usize::try_from(if range.effect.filter().is_some() {
        range.effect.warmup(rate)
    } else {
        0
    })
    .unwrap_or(0);
    let from = range.start - before as Frames;
    let n = before + len + tail;
    let dry = read(from, n);
    let weights: Vec<f32> = (from..).take(n).map(weight).collect();
    let gain = match range.effect {
        Effect::Normalize { .. } | Effect::Loudness { .. } => {
            let under = dry
                .clone()
                .map(|c| c.into_iter().skip(before).take(len).collect::<Vec<f32>>());
            measured_gain(&range.effect, &under, rate, range.start)
        }
        _ => 1.0,
    };
    let range = EffectRange { gain, ..range.clone() };
    let mut change = [vec![0.0; n], vec![0.0; n]];
    apply_range(&range, rate, &dry, &weights, &mut change, state);
    (from, change)
}

/// The gain normalize or loudness gives `audio`, the sound under a range that starts at timeline frame `start`.
fn measured_gain(effect: &Effect, audio: &[Vec<f32>; 2], rate: u32, start: Frames) -> f32 {
    let peak = audio.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    let lufs = match effect {
        Effect::Loudness { .. } => from_slices(slices(audio, rate, 0, start).iter()),
        _ => None,
    };
    gain_for(effect, peak, lufs).0
}

/// Adds a reversed effect's sound, which ends at `end`, to `out`, which holds timeline frames from `start`.
fn add_reversed(end: Frames, sound: &[Vec<f32>; 2], loop_end: Option<Frames>, start: Frames, out: &mut [Vec<f32>; 2]) {
    let len = sound[0].len() as Frames;
    let from = end - len;
    if loop_end.is_none() && (from >= start.saturating_add(out[0].len() as Frames) || end <= start) {
        return;
    }
    for (channel, sound) in out.iter_mut().zip(sound) {
        for (i, sample) in channel.iter_mut().enumerate() {
            let t = start.saturating_add(i as Frames) - from;
            // With a loop it is heard in every loop; one longer than the loop overlaps itself.
            let (mut k, step) = match loop_end {
                Some(l) => (t.rem_euclid(l), l),
                None => (t, len.max(1)),
            };
            while let Some(s) = usize::try_from(k).ok().and_then(|k| sound.get(k)) {
                *sample += s;
                k += step;
            }
        }
    }
}

/// How much of the processed sound each frame gets: 1 inside the range, fading over its edges, 0 outside.
fn range_weights(range: &EffectRange, loop_end: Option<Frames>, start: Frames, frames: usize) -> Vec<f32> {
    // A range round the whole loop has no edges: its end runs straight into its start.
    let whole_loop = loop_end.is_some_and(|l| range.end - range.start >= l);
    let (fade_in, fade_out) = (range.fade_in.max(1) as f32, range.fade_out.max(1) as f32);
    (0..frames)
        .map(|i| {
            let t = start.saturating_add(i as Frames);
            let Some(into) = position_in(range, t, loop_end) else {
                return 0.0;
            };
            let left_in = (range.end - range.start - into) as f32;
            if whole_loop {
                1.0
            } else {
                ((into as f32 + 1.0) / fade_in).min(left_in / fade_out).min(1.0)
            }
        })
        .collect()
}

fn apply_range(
    range: &EffectRange,
    rate: u32,
    dry: &[Vec<f32>; 2],
    weights: &[f32],
    out: &mut [Vec<f32>; 2],
    state: &mut State,
) {
    let wet: [Vec<f32>; 2] = match state {
        State::Nothing => dry.clone().map(|c| c.iter().map(|s| s * range.gain).collect()),
        State::Filter(stages) => {
            let mut wet = dry.clone();
            if let Some((hz, qs)) = range.effect.filter() {
                for (&q, memory) in qs.iter().zip(stages.iter_mut()) {
                    let biquad = Biquad::new(range.effect, hz, q, rate);
                    for (channel, memory) in wet.iter_mut().zip(memory.iter_mut()) {
                        biquad.run(memory, channel);
                    }
                }
            }
            wet
        }
        State::Echo { lines, at } => {
            let Effect::Echo { fade_db, level_db, .. } = range.effect else {
                return;
            };
            let (feedback, level) = (db_to_amp(-fade_db), db_to_amp(level_db));
            let len = lines[0].len().max(1);
            for ((channel, dry), line) in out.iter_mut().zip(dry).zip(lines.iter_mut()) {
                let mut i = *at;
                for ((sample, dry), weight) in channel.iter_mut().zip(dry).zip(weights) {
                    if let Some(slot) = line.get_mut(i) {
                        let delayed = *slot;
                        *slot = dry * weight + feedback * delayed;
                        *sample += level * delayed;
                    }
                    i = next(i, len);
                }
            }
            *at = (*at + weights.len()) % len;
            return;
        }
        State::Reverb(reverb) => {
            let Effect::Reverb { level_db, .. } = range.effect else {
                return;
            };
            reverb.run(dry, weights, db_to_amp(level_db), out);
            return;
        }
    };
    for ((channel, dry), wet) in out.iter_mut().zip(dry).zip(wet) {
        for (((sample, dry), wet), weight) in channel.iter_mut().zip(dry).zip(wet).zip(weights) {
            *sample += weight * (wet - dry);
        }
    }
}

/// Keeps the cutoff below the highest frequency the rate can hold.
fn cutoff(hz: f32, rate: u32) -> f32 {
    hz.min(rate as f32 * 0.45).max(1.0)
}

/// One second-order filter stage, from the formulas of the "Audio EQ Cookbook".
pub(crate) struct Biquad {
    pub(crate) b: [f64; 3],
    pub(crate) a: [f64; 2],
}

impl Biquad {
    fn new(effect: Effect, hz: f32, q: f64, rate: u32) -> Biquad {
        let w0 = std::f64::consts::TAU * f64::from(cutoff(hz, rate)) / f64::from(rate.max(1));
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = match effect {
            Effect::HighPass { .. } => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            _ => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
        };
        Biquad {
            b: b.map(|v| v / a0),
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
        }
    }

    /// Filters in place, going on from the last two inputs and outputs in `memory`.
    pub(crate) fn run(&self, memory: &mut [f64; 4], samples: &mut [f32]) {
        let [mut x1, mut x2, mut y1, mut y2] = *memory;
        for s in samples {
            let x = f64::from(*s);
            let y = self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            *s = y as f32;
        }
        *memory = [x1, x2, y1, y2];
    }
}

/// Delays of the parallel echo loops. Not multiples of each other, so their repeats do not pile up into a tone.
const COMB_MS: [f32; 8] = [25.3, 26.9, 28.9, 30.7, 32.3, 33.8, 35.3, 36.7];
/// Delays of the diffusers in a row after the loops: they smear each repeat into a cloud.
const PASS_MS: [f32; 4] = [12.6, 10.0, 7.7, 5.1];
/// The right side's loops are this much longer, so left and right differ and the room sounds wide.
const SPREAD_MS: f32 = 0.5;
/// How much of the highs each loop loses per round: real walls absorb highs first.
const DAMPING: f32 = 0.25;
const DIFFUSION: f32 = 0.5;

/// One echo loop of the room.
struct Comb {
    line: Vec<f32>,
    at: usize,
    feedback: f32,
    /// The sound going round, without its highs.
    low: f32,
}

/// The next place in a delay line of `len`. Not `%`: a division per sample made long reverbs slow.
fn next(at: usize, len: usize) -> usize {
    if at + 1 >= len { 0 } else { at + 1 }
}

/// A room after Schroeder: echo loops in parallel that lose highs, then all-pass diffusers in a row.
struct Reverb {
    combs: [Vec<Comb>; 2],
    passes: [Vec<(Vec<f32>, usize)>; 2],
    /// Brings the energy of the room's response to 1, so the volume means the same for every length.
    scale: f32,
}

impl Reverb {
    fn new(seconds: f32, rate: u32) -> Reverb {
        let line = |ms: f32| vec![0.0f32; usize::try_from(ms_frames(ms, rate)).unwrap_or(1)];
        let side = |spread: f32| {
            let combs: Vec<Comb> = COMB_MS
                .iter()
                .map(|ms| {
                    let line = line(ms + spread);
                    // Each round through the loop takes its delay; after `seconds` the sound is 60 dB down.
                    let feedback = 10f32.powf(-3.0 * line.len() as f32 / (seconds.max(0.01) * rate.max(1) as f32));
                    Comb {
                        line,
                        at: 0,
                        feedback,
                        low: 0.0,
                    }
                })
                .collect();
            let passes = PASS_MS.iter().map(|ms| (line(ms + spread), 0)).collect();
            (combs, passes)
        };
        let (left, left_passes) = side(0.0);
        let (right, right_passes) = side(SPREAD_MS);
        let energy: f32 = left
            .iter()
            .map(|c| 1.0 / (1.0 - c.feedback * c.feedback).max(1e-6))
            .sum();
        Reverb {
            combs: [left, right],
            passes: [left_passes, right_passes],
            scale: 1.0 / energy.sqrt(),
        }
    }

    /// Adds the room to `out`, fed with both sides of `dry` times `weights`.
    fn run(&mut self, dry: &[Vec<f32>; 2], weights: &[f32], level: f32, out: &mut [Vec<f32>; 2]) {
        let feed: Vec<f32> = dry[0]
            .iter()
            .zip(&dry[1])
            .zip(weights)
            .map(|((l, r), w)| (l + r) * 0.5 * w * self.scale)
            .collect();
        for ((channel, combs), passes) in out.iter_mut().zip(&mut self.combs).zip(&mut self.passes) {
            for (sample, &input) in channel.iter_mut().zip(&feed) {
                let mut room = 0.0;
                for comb in combs.iter_mut() {
                    let Some(slot) = comb.line.get_mut(comb.at) else {
                        continue;
                    };
                    let delayed = *slot;
                    comb.low = delayed * (1.0 - DAMPING) + comb.low * DAMPING;
                    *slot = input + comb.low * comb.feedback;
                    comb.at = next(comb.at, comb.line.len());
                    room += delayed;
                }
                for (line, at) in passes.iter_mut() {
                    let Some(slot) = line.get_mut(*at) else { continue };
                    let delayed = *slot;
                    let v = room + DIFFUSION * delayed;
                    *slot = v;
                    *at = next(*at, line.len());
                    room = delayed - DIFFUSION * v;
                }
                *sample += level * room;
            }
        }
    }
}

/// How far into the range timeline frame `t` is, if it is in it. With a loop, a range past the loop end
/// also covers the start of the next loop.
fn position_in(range: &EffectRange, t: Frames, loop_end: Option<Frames>) -> Option<Frames> {
    let t = loop_end.map_or(t, |l| t.rem_euclid(l));
    let shifts = [0, loop_end.unwrap_or(0)];
    shifts
        .iter()
        .take(if loop_end.is_some() { 2 } else { 1 })
        .map(|s| t + s)
        .find(|&t| t >= range.start && t < range.end)
        .map(|t| t - range.start)
}

/// Measures what the effects need from the audio under them, lane by lane from the top, so each lane
/// measures what the lanes above it produce. Where the sound under a range is the same as in `before`, the
/// project before the edit, the range keeps what it measured: rendering long audio takes seconds.
pub(crate) fn measure(project: &mut Project, before: Option<&Project>) {
    let before = before.filter(|b| b.sample_rate == project.sample_rate && b.loop_end == project.loop_end);
    for ti in 0..project.tracks.len() {
        let lanes = project.tracks.get(ti).map_or(0, |t| t.effects.len());
        for li in 0..lanes {
            let measured: Vec<Measured> = {
                let Some(track) = project.tracks.get(ti) else { continue };
                let Some(lane) = track.effects.get(li) else { continue };
                let unchanged = before.and_then(|b| b.track(track.id)).is_some_and(|old| {
                    same_clips(&old.clips, &track.clips) && same_lanes(&old.effects, &track.effects, li)
                });
                lane.ranges
                    .iter()
                    .map(|r| {
                        measure_range(r, project, unchanged, |from, frames| {
                            render_track(project, track, from, frames, li)
                        })
                    })
                    .collect()
            };
            if let Some(lane) = project.tracks.get_mut(ti).and_then(|t| t.effects.get_mut(li)) {
                lane.ranges
                    .iter_mut()
                    .zip(measured)
                    .for_each(|(r, m)| (r.gain, r.limited, r.reversed, r.heard) = m);
            }
        }
    }
    for li in 0..project.mix_effects.len() {
        let measured: Vec<Measured> = {
            let Some(lane) = project.mix_effects.get(li) else {
                continue;
            };
            let unchanged = before.is_some_and(|b| {
                b.tracks.len() == project.tracks.len()
                    && b.tracks.iter().zip(&project.tracks).all(|(old, new)| {
                        (old.id, old.gain_db, old.muted) == (new.id, new.gain_db, new.muted)
                            && same_clips(&old.clips, &new.clips)
                            && same_lanes(&old.effects, &new.effects, new.effects.len().max(old.effects.len()))
                    })
                    && same_lanes(&b.mix_effects, &project.mix_effects, li)
            });
            lane.ranges
                .iter()
                .map(|r| {
                    measure_range(r, project, unchanged, |from, frames| {
                        mix_upto(project, from, frames, li)
                    })
                })
                .collect()
        };
        if let Some(lane) = project.mix_effects.get_mut(li) {
            lane.ranges
                .iter_mut()
                .zip(measured)
                .for_each(|(r, m)| (r.gain, r.limited, r.reversed, r.heard) = m);
        }
    }
}

/// Whether the clips play the same sound. The same audio is the same shared source, so no sample is read.
fn same_clips(a: &[Clip], b: &[Clip]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            Arc::ptr_eq(&a.source, &b.source)
                && (a.id, a.start, a.offset, a.len) == (b.id, b.start, b.offset, b.len)
                && (a.fade_in, a.fade_out, a.crossfade_shape, a.channels)
                    == (b.fade_in, b.fade_out, b.crossfade_shape, b.channels)
                && a.gain == b.gain
        })
}

/// Whether the first `count` lanes change the sound in the same way.
fn same_lanes(a: &[EffectLane], b: &[EffectLane], count: usize) -> bool {
    let (Some(a), Some(b)) = (a.get(..count), b.get(..count)) else {
        return false;
    };
    a.iter().zip(b).all(|(a, b)| {
        (a.id, a.enabled) == (b.id, b.enabled)
            && a.ranges.len() == b.ranges.len()
            && a.ranges.iter().zip(&b.ranges).all(|(a, b)| {
                (a.id, a.start, a.end, a.fade_in, a.fade_out) == (b.id, b.start, b.end, b.fade_in, b.fade_out)
                    && (a.effect, a.gain, a.limited) == (b.effect, b.gain, b.limited)
                    && match (&a.reversed, &b.reversed) {
                        (Some(a), Some(b)) => Arc::ptr_eq(&a.sound, &b.sound),
                        (a, b) => a.is_none() && b.is_none(),
                    }
            })
    })
}

fn span(range: &EffectRange) -> usize {
    usize::try_from(range.end - range.start).unwrap_or(0)
}

type Measured = (f32, bool, Option<Reversed>, Option<Heard>);

/// What a normalize or loudness range measured from the sound under it, in segments on a 30 s grid of the
/// timeline. Its settings only turn these into a gain, so changing them needs no new measuring; moving an edge
/// measures only the segment there.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Heard {
    /// Shared, so undo steps and a later measuring keep one copy of an unchanged segment.
    segments: Vec<Arc<Segment>>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Segment {
    from: Frames,
    to: Frames,
    /// How much sound before `from` settled the loudness weighting; none at the range start.
    lead: Frames,
    peak: f32,
    /// Only measured for loudness.
    slices: Option<Vec<(f64, usize)>>,
}

const SEGMENT_SECONDS: Frames = 30;

/// The segments of a range as (from, to, lead).
fn segment_spans(range: &EffectRange, rate: u32) -> Vec<(Frames, Frames, Frames)> {
    let rate = Frames::from(rate.max(1));
    let size = SEGMENT_SECONDS * rate;
    let mut spans = vec![];
    let mut from = range.start;
    while from < range.end {
        let to = ((from.div_euclid(size) + 1) * size).min(range.end);
        spans.push((from, to, (from - range.start).min(rate / 2)));
        from = to;
    }
    spans
}

/// What a range needs from the sound under it, which `render(from, frames)` renders: the gain and whether the
/// peak limit held it back, or a reversed effect's sound. `unchanged` says the sound under the range is as when
/// it was last measured, where it was measured.
fn measure_range(
    range: &EffectRange,
    project: &Project,
    unchanged: bool,
    render: impl Fn(Frames, usize) -> [Vec<f32>; 2] + Sync,
) -> Measured {
    if range.effect.reversed() {
        let kept = range
            .reversed
            .as_ref()
            .filter(|r| unchanged && r.made_for == made_for(range));
        let sound = match kept {
            Some(kept) => kept.clone(),
            None => reversed_sound(range, project, render(range.start, span(range))),
        };
        return (1.0, false, Some(sound), None);
    }
    if !matches!(range.effect, Effect::Normalize { .. } | Effect::Loudness { .. }) {
        return (1.0, false, None, None);
    }
    let loudness_too = matches!(range.effect, Effect::Loudness { .. });
    let rate = project.sample_rate;
    let old = range
        .heard
        .as_ref()
        .filter(|_| unchanged)
        .map_or(&[][..], |h| &h.segments[..]);
    let segments = in_parallel(&segment_spans(range, rate), |&(from, to, lead)| {
        let kept = old
            .iter()
            .find(|s| (s.from, s.to, s.lead) == (from, to, lead) && (s.slices.is_some() || !loudness_too));
        if let Some(kept) = kept {
            return kept.clone();
        }
        let audio = render(from - lead, usize::try_from(to - from + lead).unwrap_or(0));
        let lead = usize::try_from(lead).unwrap_or(0);
        Arc::new(Segment {
            from,
            to,
            lead: lead as Frames,
            peak: audio
                .iter()
                .flat_map(|c| c.iter().skip(lead))
                .fold(0.0f32, |m, s| m.max(s.abs())),
            slices: loudness_too.then(|| slices(&audio, rate, lead, from)),
        })
    });
    let heard = Heard { segments };
    let (gain, limited) = gain_from(&range.effect, &heard);
    (gain, limited, None, Some(heard))
}

/// `work` on each item, spread over the processor's cores; the results in the items' order.
fn in_parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let per = items.len().div_ceil(cores).max(1);
    let work = &work;
    std::thread::scope(|scope| {
        let started: Vec<_> = items
            .chunks(per)
            .map(|chunk| {
                let thread =
                    std::thread::Builder::new().spawn_scoped(scope, move || chunk.iter().map(work).collect::<Vec<R>>());
                (chunk, thread)
            })
            .collect();
        // A chunk whose thread could not start or failed is done here instead.
        started
            .into_iter()
            .flat_map(|(chunk, thread)| {
                let done = thread.ok().and_then(|t| t.join().ok());
                done.unwrap_or_else(|| chunk.iter().map(work).collect())
            })
            .collect()
    })
}

/// What a reversed sound depends on besides the sound under the range.
fn made_for(range: &EffectRange) -> (Frames, Frames, Frames, Frames, Effect) {
    (range.start, range.end, range.fade_in, range.fade_out, range.effect)
}

/// Reverses the sound under the range, runs the effect over it and reverses the result, so the tail comes
/// first. It ends at the range end and starts one tail before the range start. When nothing it is computed
/// from changed, the range keeps the sound it has: undo steps then share it instead of each holding a copy.
fn reversed_sound(range: &EffectRange, project: &Project, mut audio: [Vec<f32>; 2]) -> Reversed {
    use std::hash::{Hash, Hasher};
    let rate = project.sample_rate;
    let len = audio[0].len();
    let weights = range_weights(range, project.loop_end.filter(|l| *l > 0), range.start, len);
    for channel in &mut audio {
        channel.iter_mut().zip(&weights).for_each(|(s, w)| *s *= w);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (format!("{:?}", range.effect), rate).hash(&mut hasher);
    audio.iter().flatten().for_each(|s| s.to_bits().hash(&mut hasher));
    let key = hasher.finish();
    if let Some(kept) = range.reversed.as_ref().filter(|r| r.key == key) {
        return Reversed {
            made_for: made_for(range),
            ..kept.clone()
        };
    }
    let total = len.saturating_add(usize::try_from(range.effect.tail(rate)).unwrap_or(0));
    for channel in &mut audio {
        channel.reverse();
        channel.resize(total, 0.0);
    }
    let mut sound = [vec![0.0; total], vec![0.0; total]];
    let state = &mut State::new(range.effect, rate);
    apply_range(range, rate, &audio, &vec![1.0; total], &mut sound, state);
    sound.iter_mut().for_each(|c| c.reverse());
    Reversed {
        key,
        made_for: made_for(range),
        sound: Arc::new(sound),
    }
}

/// The gain for normalize and loudness, and whether the peak limit held it back.
fn gain_from(effect: &Effect, heard: &Heard) -> (f32, bool) {
    let peak = heard.segments.iter().fold(0.0f32, |m, s| m.max(s.peak));
    let lufs = match effect {
        Effect::Loudness { .. } => from_slices(heard.segments.iter().flat_map(|s| s.slices.iter().flatten())),
        _ => None,
    };
    gain_for(effect, peak, lufs)
}

/// The gain for normalize and loudness from the loudest sample and, for loudness, the loudness of the sound
/// under the range; and whether the peak limit held it back. Silence is left alone.
fn gain_for(effect: &Effect, peak: f32, lufs: Option<f64>) -> (f32, bool) {
    if peak <= 1e-6 {
        return (1.0, false);
    }
    let most = db_to_amp(peak_db_of(effect)) / peak;
    let (gain, limited) = match (*effect, lufs) {
        (Effect::Loudness { lufs: target, .. }, Some(now)) => {
            let wanted = db_to_amp(target - now as f32);
            (wanted.min(most), wanted > most)
        }
        (Effect::Loudness { .. }, None) => (1.0, false),
        _ => (most, false),
    };
    (gain.clamp(db_to_amp(-80.0), db_to_amp(40.0)), limited)
}

fn peak_db_of(effect: &Effect) -> f32 {
    match *effect {
        Effect::Normalize { peak_db } | Effect::Loudness { peak_db, .. } => peak_db,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::loudness::loudness;
    use crate::{Command, GainPoint, RangeId, Session, Source, TrackId, mixdown, render};

    const RATE: u32 = 1000;

    /// A constant signal, so the peak is easy to check everywhere.
    fn level(value: f32, frames: usize) -> Arc<Source> {
        Arc::new(Source {
            path: None,
            name: "level".into(),
            sample_rate: RATE,
            channels: vec![vec![value; frames]],
            generated: None,
        })
    }

    fn session(clips: &[(f32, Frames)]) -> (Session, TrackId) {
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        for &(value, start) in clips {
            s.execute(Command::AddClip {
                track,
                source: level(value, 1000),
                start,
            })
            .unwrap();
        }
        (s, track)
    }

    fn add_lane(s: &mut Session, track: Option<TrackId>, peak_db: f32, ranges: &[(Frames, Frames)]) -> LaneId {
        s.execute(Command::AddEffectLane {
            track,
            effect: Effect::Normalize { peak_db },
        })
        .unwrap();
        let p = s.project();
        let lane = match track {
            Some(t) => p.track(t).unwrap().effects.last().unwrap().id,
            None => p.mix_effects.last().unwrap().id,
        };
        for &(start, end) in ranges {
            s.execute(Command::AddEffectRange { lane, start, end }).unwrap();
        }
        lane
    }

    #[test]
    fn normalize_across_two_clips_from_inside_one_to_inside_the_other() {
        // 0.25 from 0 to 1000, 0.5 from 1000 to 2000.
        let (mut s, track) = session(&[(0.25, 0), (0.5, 1000)]);
        add_lane(&mut s, Some(track), -6.0, &[(500, 1500)]);
        let [left, _] = render(s.project(), 0, 2000);
        let target = db_to_amp(-6.0);
        assert!((left[400] - 0.25).abs() < 1e-6, "before the range: untouched");
        assert!((left[1200] - target).abs() < 1e-4, "{}", left[1200]);
        assert!(
            (left[700] - 0.25 * target / 0.5).abs() < 1e-4,
            "same gain for both clips: {}",
            left[700]
        );
        assert!((left[1700] - 0.5).abs() < 1e-6, "after the range: untouched");
        assert!(left.iter().all(|v| *v <= target + 1e-4), "nothing goes past the peak");
    }

    #[test]
    fn a_range_fades_its_effect_in_and_out_over_its_set_fades() {
        let (mut s, track) = session(&[(0.25, 0)]);
        let lane = add_lane(&mut s, Some(track), -6.0, &[(0, 1000)]);
        let range = s.project().lane(lane).unwrap().ranges[0].id;
        s.execute(Command::SetRangeFades {
            range,
            fade_in: 400,
            fade_out: 200,
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 1000);
        let target = db_to_amp(-6.0);
        let halfway = 0.25 + 0.5 * (target - 0.25);
        assert!((left[199] - halfway).abs() < 1e-4, "half faded in: {}", left[199]);
        assert!((left[600] - target).abs() < 1e-4, "{}", left[600]);
        assert!(
            (left[899] - (0.25 + 0.505 * (target - 0.25))).abs() < 1e-4,
            "{}",
            left[899]
        );

        // The fades cannot pass each other, and a shorter range shortens both alike.
        let overlapping = Command::SetRangeFades {
            range,
            fade_in: 900,
            fade_out: 200,
        };
        assert!(s.execute(overlapping).is_err());
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 300,
        })
        .unwrap();
        let r = &s.project().lane(lane).unwrap().ranges[0];
        assert_eq!((r.fade_in, r.fade_out), (200, 100));
    }

    #[test]
    fn effects_come_before_the_track_volume_and_the_mix_lanes_after_all_tracks() {
        let (mut s, track) = session(&[(0.25, 0)]);
        add_lane(&mut s, Some(track), -1.0, &[(0, 1000)]);
        s.execute(Command::SetTrackGain { track, db: -6.0 }).unwrap();
        let [left, _] = render(s.project(), 0, 1000);
        assert!((left[500] - db_to_amp(-7.0)).abs() < 1e-4, "{}", left[500]);

        s.execute(Command::AddTrack { name: "u".into() }).unwrap();
        let other = s.project().tracks[1].id;
        s.execute(Command::AddClip {
            track: other,
            source: level(0.1, 1000),
            start: 0,
        })
        .unwrap();
        add_lane(&mut s, None, -3.0, &[(0, 1000)]);
        let [left, _] = render(s.project(), 0, 1000);
        assert!((left[500] - db_to_amp(-3.0)).abs() < 1e-4, "{}", left[500]);
    }

    #[test]
    fn a_range_past_the_loop_end_also_covers_the_start_and_a_whole_loop_has_no_seam() {
        let (mut s, track) = session(&[(0.25, 0)]);
        s.execute(Command::SetLoopEnd { at: Some(1000) }).unwrap();
        let lane = add_lane(&mut s, Some(track), -6.0, &[(800, 1200)]);
        let [left, _] = render(s.project(), 0, 1000);
        let loud = 0.25 * db_to_amp(-6.0) / 0.25;
        assert!((left[100] - loud).abs() < 1e-4, "{}", left[100]);
        assert!((left[500] - 0.25).abs() < 1e-6);

        let range = s.project().lane(lane).unwrap().ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 1000,
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 1000);
        assert!(left.iter().all(|v| (v - loud).abs() < 1e-4), "no dip at the seam");
    }

    #[test]
    fn ranges_in_one_lane_cannot_overlap() {
        let (mut s, track) = session(&[(0.25, 0)]);
        let lane = add_lane(&mut s, Some(track), -6.0, &[(0, 500)]);
        assert!(
            s.execute(Command::AddEffectRange {
                lane,
                start: 400,
                end: 900
            })
            .is_err()
        );
        assert!(
            s.execute(Command::AddEffectRange {
                lane,
                start: 500,
                end: 900
            })
            .is_ok()
        );
    }

    fn tone_session(hz: f32, effect: Effect) -> Session {
        let rate = 48_000;
        let samples = (0..rate)
            .map(|i| 0.5 * (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin())
            .collect();
        let source = Arc::new(Source {
            path: None,
            name: "tone".into(),
            sample_rate: rate,
            channels: vec![samples],
            generated: None,
        });
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source,
            start: 0,
        })
        .unwrap();
        s.execute(Command::AddEffectLane {
            track: Some(track),
            effect,
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[0].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 0,
            end: 48_000,
        })
        .unwrap();
        s
    }

    /// The loudest sample in the middle of the second, after the filter settled.
    fn level_after(s: &Session) -> f32 {
        render(s.project(), 12_000, 24_000)[0]
            .iter()
            .fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn high_pass_removes_hum_and_low_pass_removes_hiss() {
        let high = Effect::HighPass {
            hz: 1000.0,
            steep: false,
        };
        let low = Effect::LowPass {
            hz: 1000.0,
            steep: false,
        };
        assert!(level_after(&tone_session(50.0, high)) < 0.01);
        // 6 kHz: at 48 kHz its samples hit the peak of the wave.
        assert!(level_after(&tone_session(6000.0, high)) > 0.48);
        assert!(level_after(&tone_session(50.0, low)) > 0.48);
        assert!(level_after(&tone_session(6000.0, low)) < 0.02);

        let gentle = level_after(&tone_session(4000.0, low));
        let steep = level_after(&tone_session(
            4000.0,
            Effect::LowPass {
                hz: 1000.0,
                steep: true,
            },
        ));
        assert!(steep < gentle / 8.0, "{steep} {gentle}");
    }

    /// Playback renders in pieces; with the warm-up they join like one long render, also over the loop seam.
    #[test]
    fn rendering_in_pieces_matches_one_render() {
        let mut s = tone_session(3000.0, Effect::LowPass { hz: 500.0, steep: true });
        let whole = render(s.project(), 0, 48_000);
        let mut pieces = vec![];
        for start in (0..48_000).step_by(512) {
            pieces.extend(render(s.project(), start, 512.min(48_000 - start as usize))[0].clone());
        }
        let most = whole[0]
            .iter()
            .zip(&pieces)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(most < 1e-3, "{most}");

        let range = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 30_000,
        })
        .unwrap();
        s.execute(Command::SetLoopEnd { at: Some(30_000) }).unwrap();
        let across = render(s.project(), 29_744, 512);
        let mut joined = render(s.project(), 29_744, 256)[0].clone();
        joined.extend(render(s.project(), 0, 256)[0].clone());
        let most = across[0]
            .iter()
            .zip(&joined)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(most < 1e-3, "{most}");
    }

    /// One click at frame 100 on a track, with a lane of `effect` over 0..200.
    fn click_session(effect: Effect) -> Session {
        let mut samples = vec![0.0; 300];
        samples[100] = 1.0;
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        let source = Arc::new(Source {
            path: None,
            name: "click".into(),
            sample_rate: RATE,
            channels: vec![samples],
            generated: None,
        });
        s.execute(Command::AddClip {
            track,
            source,
            start: 0,
        })
        .unwrap();
        s.execute(Command::AddEffectLane {
            track: Some(track),
            effect,
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[0].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 0,
            end: 200,
        })
        .unwrap();
        s
    }

    #[test]
    fn echo_repeats_quieter_each_time_and_sounds_past_the_range() {
        let echo = Effect::Echo {
            delay_ms: 100.0,
            fade_db: 6.0,
            level_db: -6.0,
            reversed: false,
        };
        let s = click_session(echo);
        // The first echo at -6 dB, each next one 6 dB quieter, until 60 dB down: 10 repeats, 1 s.
        assert_eq!(echo.tail(RATE), 1000);
        assert_eq!(s.project().len(), 1200);
        let [left, _] = render(s.project(), 0, 1200);
        assert_eq!(left[100], 1.0);
        for (k, at) in [200, 300, 400].into_iter().enumerate() {
            let expected = db_to_amp(-6.0 * (k as f32 + 1.0));
            assert!((left[at] - expected).abs() < 1e-5, "{at}: {}", left[at]);
        }
        assert!(left[250].abs() < 1e-6);
    }

    #[test]
    fn reverb_dies_away_after_the_range() {
        let s = click_session(Effect::Reverb {
            seconds: 0.5,
            level_db: 0.0,
            reversed: false,
        });
        let [left, right] = render(s.project(), 0, 1200);
        let peak = |from: usize, to: usize| left[from..to].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let early = peak(200, 300);
        assert!(early > 0.01, "the room sounds after the range: {early}");
        assert!(peak(700, 800) < early / 100.0, "{}", peak(700, 800));
        assert_ne!(left[250], right[250], "left and right differ, so it sounds wide");
    }

    /// Playback goes on from the effects' memory instead of rendering the past again; the result is the
    /// same as one render, and with the loop on, the tail runs over the seam into the start.
    #[test]
    fn playback_in_pieces_matches_one_render() {
        let mut s = click_session(Effect::Echo {
            delay_ms: 70.0,
            fade_db: 4.0,
            level_db: -3.0,
            reversed: false,
        });
        let track = s.project().tracks[0].id;
        for effect in [
            Effect::Reverb {
                seconds: 0.3,
                level_db: -6.0,
                reversed: false,
            },
            Effect::LowPass { hz: 200.0, steep: true },
            Effect::Reverb {
                seconds: 0.2,
                level_db: -6.0,
                reversed: true,
            },
        ] {
            s.execute(Command::AddEffectLane {
                track: Some(track),
                effect,
            })
            .unwrap();
            let lane = s.project().tracks[0].effects.last().unwrap().id;
            s.execute(Command::AddEffectRange {
                lane,
                start: 50,
                end: 250,
            })
            .unwrap();
        }
        let differs = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max);
        let played = |s: &Session, from: usize, to: usize| {
            let mut renderer = crate::Renderer::default();
            let mut out = vec![];
            for start in (0..to).step_by(64) {
                let at = s.project().loop_end.map_or(start as Frames, |l| start as Frames % l);
                let piece = renderer.render(s.project(), at, 64)[0].clone();
                out.extend(piece);
            }
            out[from..to].to_vec()
        };
        // Playback stops an echo once it is 60 dB down; one render keeps going, so they match to -60 dB.
        let whole = render(s.project(), 0, 1500)[0].clone();
        assert!(differs(&whole, &played(&s, 0, 1500)) < 1e-3);

        s.execute(Command::SetLoopEnd { at: Some(600) }).unwrap();
        let whole = render(s.project(), 0, 600)[0].clone();
        // The 8th echo comes at 660, which is 60 in the next loop.
        assert!(
            whole[50..100].iter().any(|v| v.abs() > 1e-3),
            "the tail wraps into the start"
        );
        // Once playback has run longer than the echo's tail (1120 frames, almost two loops), each loop sounds
        // like the exported one.
        assert!(differs(&whole, &played(&s, 1800, 2400)) < 1e-3);
    }

    #[test]
    fn each_range_keeps_its_own_settings_and_new_ones_start_with_the_last_set() {
        let echo = |delay_ms| Effect::Echo {
            delay_ms,
            fade_db: 6.0,
            level_db: -6.0,
            reversed: false,
        };
        let mut s = click_session(echo(100.0));
        let lane = s.project().tracks[0].effects[0].id;
        let first = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 500,
            end: 600,
        })
        .unwrap();
        s.execute(Command::SetEffect {
            range: first,
            effect: echo(50.0),
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 300);
        assert!(left[150] > 0.4 && left[200] < 0.3, "the first range echoes after 50 ms");
        s.execute(Command::AddEffectRange {
            lane,
            start: 800,
            end: 900,
        })
        .unwrap();
        let effects = |s: &Session| -> Vec<Effect> {
            s.project().tracks[0].effects[0]
                .ranges
                .iter()
                .map(|r| r.effect)
                .collect()
        };
        assert_eq!(effects(&s), [echo(50.0), echo(100.0), echo(50.0)]);
        s.execute(Command::SetLaneEffect {
            lane,
            effect: echo(80.0),
        })
        .unwrap();
        assert_eq!(effects(&s), [echo(80.0); 3]);
        assert!(
            s.execute(Command::SetEffect {
                range: first,
                effect: Effect::Normalize { peak_db: -1.0 }
            })
            .is_err(),
            "a lane holds one kind"
        );
    }

    #[test]
    fn dragging_the_tail_sets_the_reverb_length_or_the_echo_repeats() {
        let reverb = Effect::Reverb {
            seconds: 1.5,
            level_db: -12.0,
            reversed: false,
        };
        let longer = reverb.with_tail(reverb.tail(48_000) + 24_000, 48_000);
        assert_eq!(
            longer,
            Effect::Reverb {
                seconds: 2.0,
                level_db: -12.0,
                reversed: false,
            }
        );

        let echo = Effect::Echo {
            delay_ms: 100.0,
            fade_db: 6.0,
            level_db: -6.0,
            reversed: false,
        };
        assert_eq!(echo.tail(RATE), 1000);
        for (dragged, repeats) in [(640, 6), (400, 4), (10, 3), (90_000, 19)] {
            let set = echo.with_tail(dragged, RATE);
            assert_eq!(set.tail(RATE), repeats * 100, "{dragged}: {set:?}");
            assert!(set.check().is_ok());
        }
    }

    #[test]
    fn loudness_reaches_its_target_unless_the_peak_limit_holds_it_back() {
        // The middle, away from the range's edges, where the gain fades in and out.
        let lufs_of = |s: &Session| loudness(&render(s.project(), 12_000, 24_000), 48_000).unwrap();
        let s = tone_session(
            997.0,
            Effect::Loudness {
                lufs: -16.0,
                peak_db: -1.0,
            },
        );
        assert!((lufs_of(&s) + 16.0).abs() < 0.1, "{}", lufs_of(&s));
        assert!(!s.project().tracks[0].effects[0].ranges[0].limited);

        // The tone is at -6 dB, so 0 LUFS needs +6 dB, which would take the peak to 0 dB.
        let s = tone_session(
            997.0,
            Effect::Loudness {
                lufs: 0.0,
                peak_db: -1.0,
            },
        );
        let range = &s.project().tracks[0].effects[0].ranges[0];
        assert!(range.limited);
        let peak = render(s.project(), 0, 48_000)[0]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - db_to_amp(-1.0)).abs() < 1e-3, "{peak}");
    }

    /// Measuring the audio under a range took seconds on every edit; now it is kept while that audio stays the
    /// same. What is kept must always match a fresh measurement.
    #[test]
    fn kept_measurements_match_fresh_ones() {
        let gains = |p: &Project| -> Vec<(f32, bool)> {
            let lanes = p.tracks.iter().flat_map(|t| &t.effects).chain(&p.mix_effects);
            lanes.flat_map(|l| &l.ranges).map(|r| (r.gain, r.limited)).collect()
        };
        let check = |s: &Session| {
            let fresh = Session::open(Project::clone(s.project())).unwrap();
            assert_eq!(gains(s.project()), gains(fresh.project()));
        };
        let mut s = tone_session(
            997.0,
            Effect::Loudness {
                lufs: -16.0,
                peak_db: -1.0,
            },
        );
        let track = s.project().tracks[0].id;
        add_lane(&mut s, None, -3.0, &[(0, 48_000)]);
        let before = gains(s.project());
        let range = s.project().tracks[0].effects[0].ranges[0].id;
        let louder = Effect::Loudness {
            lufs: -12.0,
            peak_db: -1.0,
        };
        s.execute(Command::SetEffect { range, effect: louder }).unwrap();
        check(&s);
        let after_settings = gains(s.project());
        assert_ne!(before, after_settings);

        // Under the track's range: the clip gets quieter.
        let clip = s.project().tracks[0].clips[0].id;
        let points = vec![GainPoint { at: 0, db: -20.0 }];
        s.execute(Command::SetGainCurve { clip, points }).unwrap();
        check(&s);
        assert_ne!(after_settings, gains(s.project()));

        // Only under the mix range: the track volume comes after the track's effects.
        let after_curve = gains(s.project());
        s.execute(Command::SetTrackGain { track, db: -10.0 }).unwrap();
        check(&s);
        assert_ne!(after_curve, gains(s.project()));
    }

    /// A 70 s range at 8 kHz from 0.2 s: a 60 Hz hum the weighting filter must settle on, a tone with steps
    /// in level, and a pause.
    fn long_loudness_session() -> (Session, RangeId) {
        let rate = 8000;
        let samples = (0..70 * rate)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let step = [0.5, 0.1, 0.0, 0.3][(i / (17 * rate)) as usize % 4];
                0.2 * (t * 60.0 * std::f32::consts::TAU).sin() + step * (t * 1000.0 * std::f32::consts::TAU).sin()
            })
            .collect();
        let source = Arc::new(Source {
            path: None,
            name: "long".into(),
            sample_rate: rate,
            channels: vec![samples],
            generated: None,
        });
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        s.execute(Command::AddClip {
            track,
            source,
            start: 0,
        })
        .unwrap();
        let effect = Effect::Loudness {
            lufs: -16.0,
            peak_db: 0.0,
        };
        s.execute(Command::AddEffectLane {
            track: Some(track),
            effect,
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[0].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 1600,
            end: 69 * 8000,
        })
        .unwrap();
        let range = s.project().tracks[0].effects[0].ranges[0].id;
        (s, range)
    }

    #[test]
    fn loudness_measured_in_segments_matches_one_measurement() {
        let (s, _) = long_loudness_session();
        let p = s.project();
        let range = &p.tracks[0].effects[0].ranges[0];
        let heard = range.heard.as_ref().unwrap();
        assert_eq!(heard.segments.len(), 3, "cut at 30 s and 60 s");
        let segmented = from_slices(heard.segments.iter().flat_map(|s| s.slices.iter().flatten())).unwrap();
        let audio = render_track(p, &p.tracks[0], range.start, span(range), 0);
        let whole = from_slices(slices(&audio, 8000, 0, range.start).iter()).unwrap();
        assert!((segmented - whole).abs() < 1e-4, "{segmented} against {whole}");
        let peak = audio.iter().flatten().fold(0.0f32, |m, v| m.max(v.abs()));
        assert_eq!(heard.segments.iter().fold(0.0f32, |m, s| m.max(s.peak)), peak);
    }

    #[test]
    fn moving_a_range_end_measures_only_the_segment_there() {
        let (mut s, range) = long_loudness_session();
        let segments = |s: &Session| {
            s.project().tracks[0].effects[0].ranges[0]
                .heard
                .clone()
                .unwrap()
                .segments
        };
        let before = segments(&s);
        s.execute(Command::SetEffectRange {
            range,
            start: 1600,
            end: 65 * 8000,
        })
        .unwrap();
        let after = segments(&s);
        assert!(Arc::ptr_eq(&before[0], &after[0]) && Arc::ptr_eq(&before[1], &after[1]));
        assert!(!Arc::ptr_eq(&before[2], &after[2]));
        let fresh = Session::open(Project::clone(s.project())).unwrap();
        let gain = |s: &Session| s.project().tracks[0].effects[0].ranges[0].gain;
        assert_eq!(gain(&s), gain(&fresh));
    }

    /// The whole-project mixdown sounds like the renderer that plays piece by piece, for every effect, reversed,
    /// stacked, across tracks, and round a loop.
    #[test]
    fn mixdown_matches_the_piece_renderer() {
        let echo = |reversed| Effect::Echo {
            delay_ms: 30.0,
            fade_db: 6.0,
            level_db: -6.0,
            reversed,
        };
        let reverb = |reversed| Effect::Reverb {
            seconds: 0.3,
            level_db: -6.0,
            reversed,
        };
        let filters = [
            Effect::HighPass { hz: 200.0, steep: true },
            Effect::LowPass {
                hz: 400.0,
                steep: false,
            },
        ];
        let mut sessions: Vec<(&str, Session)> = vec![];
        for (name, effect) in [
            ("echo", echo(false)),
            ("reversed echo", echo(true)),
            ("reverb", reverb(false)),
            ("reversed reverb", reverb(true)),
        ] {
            sessions.push((name, click_session(effect)));
            let mut looped = click_session(effect);
            looped.execute(Command::SetLoopEnd { at: Some(600) }).unwrap();
            sessions.push((name, looped));
        }
        for effect in filters.into_iter().chain([Effect::Loudness {
            lufs: -20.0,
            peak_db: -1.0,
        }]) {
            sessions.push(("tone", tone_session(997.0, effect)));
        }
        let (mut stacked, track) = session(&[(0.25, 0), (0.5, 1500)]);
        add_lane(&mut stacked, Some(track), -6.0, &[(200, 900), (1200, 2400)]);
        stacked.execute(Command::AddTrack { name: "u".into() }).unwrap();
        let other = stacked.project().tracks[1].id;
        stacked
            .execute(Command::AddClip {
                track: other,
                source: level(0.1, 1000),
                start: 300,
            })
            .unwrap();
        add_lane(&mut stacked, None, -3.0, &[(0, 2500)]);
        sessions.push(("stacked", stacked));
        let (mut round, track) = session(&[(0.25, 0)]);
        round.execute(Command::SetLoopEnd { at: Some(1000) }).unwrap();
        add_lane(&mut round, Some(track), -6.0, &[(800, 1200)]);
        sessions.push(("normalize round the loop", round));

        for (name, s) in sessions {
            let p = s.project();
            let len = p.len() as usize;
            let (new, old) = (mixdown(p, || true).unwrap(), render(p, 0, len));
            for (new, old) in new.iter().zip(&old) {
                assert_eq!(new.len(), old.len(), "{name}");
                let off = new.iter().zip(old).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
                assert!(off < 1e-3, "{name}: off by {off}");
            }
        }
    }

    #[test]
    fn a_reversed_echo_comes_before_the_sound_and_wraps_to_the_loop_end() {
        // A click at 100 in a range from 0 to 200.
        let mut s = click_session(Effect::Echo {
            delay_ms: 30.0,
            fade_db: 6.0,
            level_db: -6.0,
            reversed: true,
        });
        let [left, _] = render(s.project(), 0, 300);
        let at = |i: usize, db: f32| (left[i] - db_to_amp(db)).abs() < 1e-4;
        assert!(at(70, -6.0) && at(40, -12.0) && at(10, -18.0), "{:?}", &left[..101]);
        assert!((left[100] - 1.0).abs() < 1e-6, "the click itself");
        assert!(left[101..].iter().all(|v| v.abs() < 1e-6), "nothing after the click");

        let lane = s.project().tracks[0].effects[0].id;
        s.execute(Command::SetLaneEffect {
            lane,
            effect: Effect::Echo {
                delay_ms: 60.0,
                // Shorter than the loop, so the wrapped part does not overlap its start.
                fade_db: 12.0,
                level_db: -6.0,
                reversed: true,
            },
        })
        .unwrap();
        s.execute(Command::SetLoopEnd { at: Some(600) }).unwrap();
        let [left, _] = render(s.project(), 0, 600);
        let at = |i: usize, db: f32| (left[i] - db_to_amp(db)).abs() < 1e-4;
        assert!(at(40, -6.0) && at(580, -18.0) && at(520, -30.0));
    }

    #[test]
    fn an_edit_elsewhere_keeps_the_reversed_sound_instead_of_a_new_copy_per_undo_step() {
        let mut s = click_session(Effect::Reverb {
            seconds: 0.5,
            level_db: -6.0,
            reversed: true,
        });
        let sound = |s: &Session| s.project().tracks[0].effects[0].ranges[0].reversed.clone().unwrap();
        let before = sound(&s);
        s.execute(Command::AddTrack { name: "other".into() }).unwrap();
        assert!(
            Arc::ptr_eq(&before.sound, &sound(&s).sound),
            "shared with the undo step before"
        );

        let range = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 0,
            end: 150,
        })
        .unwrap();
        assert!(
            !Arc::ptr_eq(&before.sound, &sound(&s).sound),
            "computed again for the new range"
        );
    }
}
