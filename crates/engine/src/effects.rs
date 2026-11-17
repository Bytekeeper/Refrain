//! Effects in lanes: each lane is one effect applied in its time ranges, lanes in order from the top.

use std::collections::HashMap;

use crate::mix::{db_to_amp, mix_upto, render_track};
use crate::{Error, Frames, Project, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaneId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RangeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Sets the gain of each range so its loudest sample reaches `peak_db`.
    Normalize { peak_db: f32 },
    /// Lets through what is above `hz`: removes rumble and hum. `steep` falls 24 dB per octave instead of 12.
    HighPass { hz: f32, steep: bool },
    /// Lets through what is below `hz`: removes hiss and harshness.
    LowPass { hz: f32, steep: bool },
    /// Repeats the sound every `delay_ms`; the first repeat at `level_db`, each next one `fade_db` quieter.
    Echo { delay_ms: f32, fade_db: f32, level_db: f32 },
    /// The sound of a room that dies away to -60 dB in `seconds`, added at `level_db`.
    Reverb { seconds: f32, level_db: f32 },
}

impl Effect {
    /// One of each kind with its usual settings, in menu order.
    pub fn defaults() -> Vec<Effect> {
        vec![
            Effect::Normalize { peak_db: -1.0 },
            Effect::HighPass { hz: 80.0, steep: false },
            Effect::LowPass {
                hz: 8000.0,
                steep: false,
            },
            Effect::Echo {
                delay_ms: 300.0,
                fade_db: 6.0,
                level_db: -6.0,
            },
            Effect::Reverb {
                seconds: 1.5,
                level_db: -12.0,
            },
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Effect::Normalize { .. } => "Normalize",
            Effect::HighPass { .. } => "High-pass",
            Effect::LowPass { .. } => "Low-pass",
            Effect::Echo { .. } => "Echo",
            Effect::Reverb { .. } => "Reverb",
        }
    }

    /// How long after a range the effect still sounds: until it is 60 dB below the sound that went in.
    pub fn tail(&self, rate: u32) -> Frames {
        match *self {
            Effect::Normalize { .. } | Effect::HighPass { .. } | Effect::LowPass { .. } => 0,
            Effect::Echo {
                delay_ms,
                fade_db,
                level_db,
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
            Effect::Reverb { level_db, .. } => {
                let extra = ms_frames(COMB_MS[7] + SPREAD_MS, rate);
                let seconds = (frames - extra) as f32 / rate.max(1) as f32;
                Effect::Reverb {
                    seconds: (seconds * 100.0).round().clamp(10.0, 1000.0) / 100.0,
                    level_db,
                }
            }
            Effect::Echo { delay_ms, level_db, .. } => {
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
                }
            }
            _ => self,
        }
    }

    /// How much audio before a frame the effect needs to compute it. Rendering starts this much earlier, so a
    /// render from the middle sounds like one from the start. For a filter its memory of the past fades to
    /// -80 dB in this time.
    pub(crate) fn warmup(&self, rate: u32) -> Frames {
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
    /// Measured from the audio under the range whenever the project changes; for normalize the gain.
    pub gain: f32,
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
fn edge_frames(rate: u32, len: Frames) -> Frames {
    (Frames::from(rate) / 100).min(len / 2).max(1)
}

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
            Effect::Normalize { .. } => State::Nothing,
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
        let weights = range_weights(range, rate, loop_end, start, dry[0].len());
        apply_range(range, rate, &dry, &weights, out, memory.of(range, rate));
    }
}

/// How much of the processed sound each frame gets: 1 inside the range, fading over its edges, 0 outside.
fn range_weights(range: &EffectRange, rate: u32, loop_end: Option<Frames>, start: Frames, frames: usize) -> Vec<f32> {
    // A range round the whole loop has no edges: its end runs straight into its start.
    let whole_loop = loop_end.is_some_and(|l| range.end - range.start >= l);
    let edge = edge_frames(rate, range.end - range.start) as f32;
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
                ((into as f32 + 1.0) / edge).min(left_in / edge).min(1.0)
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
                    i = (i + 1) % len;
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
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
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
    fn run(&self, memory: &mut [f64; 4], samples: &mut [f32]) {
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
                    comb.at = (comb.at + 1) % comb.line.len();
                    room += delayed;
                }
                for (line, at) in passes.iter_mut() {
                    let Some(slot) = line.get_mut(*at) else { continue };
                    let delayed = *slot;
                    let v = room + DIFFUSION * delayed;
                    *slot = v;
                    *at = (*at + 1) % line.len();
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
/// measures what the lanes above it produce.
pub(crate) fn measure(project: &mut Project) {
    for ti in 0..project.tracks.len() {
        let lanes = project.tracks.get(ti).map_or(0, |t| t.effects.len());
        for li in 0..lanes {
            let gains: Vec<f32> = {
                let Some(track) = project.tracks.get(ti) else { continue };
                let Some(lane) = track.effects.get(li) else { continue };
                lane.ranges
                    .iter()
                    .map(|r| normalize_gain(&r.effect, || render_track(project, track, r.start, span(r), li)))
                    .collect()
            };
            if let Some(lane) = project.tracks.get_mut(ti).and_then(|t| t.effects.get_mut(li)) {
                lane.ranges.iter_mut().zip(gains).for_each(|(r, g)| r.gain = g);
            }
        }
    }
    for li in 0..project.mix_effects.len() {
        let gains: Vec<f32> = {
            let Some(lane) = project.mix_effects.get(li) else {
                continue;
            };
            lane.ranges
                .iter()
                .map(|r| normalize_gain(&r.effect, || mix_upto(project, r.start, span(r), li)))
                .collect()
        };
        if let Some(lane) = project.mix_effects.get_mut(li) {
            lane.ranges.iter_mut().zip(gains).for_each(|(r, g)| r.gain = g);
        }
    }
}

fn span(range: &EffectRange) -> usize {
    usize::try_from(range.end - range.start).unwrap_or(0)
}

/// The gain that brings the loudest sample to the target; silence is left alone. Other effects measure nothing.
fn normalize_gain(effect: &Effect, audio: impl FnOnce() -> [Vec<f32>; 2]) -> f32 {
    let Effect::Normalize { peak_db } = *effect else {
        return 1.0;
    };
    let peak = audio().iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak <= 1e-6 {
        return 1.0;
    }
    (db_to_amp(peak_db) / peak).clamp(db_to_amp(-80.0), db_to_amp(40.0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Command, Session, Source, TrackId, render};

    const RATE: u32 = 1000;

    /// A constant signal, so the peak is easy to check everywhere.
    fn level(value: f32, frames: usize) -> Arc<Source> {
        Arc::new(Source {
            path: None,
            name: "level".into(),
            sample_rate: RATE,
            channels: vec![vec![value; frames]],
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
        });
        let track = s.project().tracks[0].id;
        for effect in [
            Effect::Reverb {
                seconds: 0.3,
                level_db: -6.0,
            },
            Effect::LowPass { hz: 200.0, steep: true },
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
        };
        let longer = reverb.with_tail(reverb.tail(48_000) + 24_000, 48_000);
        assert_eq!(
            longer,
            Effect::Reverb {
                seconds: 2.0,
                level_db: -12.0
            }
        );

        let echo = Effect::Echo {
            delay_ms: 100.0,
            fade_db: 6.0,
            level_db: -6.0,
        };
        assert_eq!(echo.tail(RATE), 1000);
        for (dragged, repeats) in [(640, 6), (400, 4), (10, 3), (90_000, 19)] {
            let set = echo.with_tail(dragged, RATE);
            assert_eq!(set.tail(RATE), repeats * 100, "{dragged}: {set:?}");
            assert!(set.check().is_ok());
        }
    }
}
