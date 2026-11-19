//! Effects in lanes: each lane is one effect applied in its time ranges, lanes in order from the top.

use crate::loudness::{from_slices, slices};
use std::collections::HashMap;

use crate::mix::{db_to_amp, read};
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
    /// Raises or lowers each of the octave bands in [`EQ_BANDS_HZ`] by its gain.
    Eq { gains_db: [f32; 10] },
    /// Keeps every sample at or below `ceiling_db`: turns the sound down smoothly just before a peak that
    /// would pass it, and back up over `release_ms`.
    Limiter { ceiling_db: f32, release_ms: f32 },
    /// Moves the pitch up or down by `semitones`; the length and timing stay.
    Pitch { semitones: f32 },
    /// Replaces the range with what the sound around it predicts: removes a click or a short blip.
    Repair,
}

/// The centres of the EQ's bands, an octave apart, as on a classic graphic EQ.
pub const EQ_BANDS_HZ: [f32; 10] = [
    31.25, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16_000.0,
];

/// How far ahead the limiter starts turning down for a peak: short enough to keep the attack of drums and
/// clicks, long enough that the turn down itself is not heard as a click.
const LOOKAHEAD_MS: f32 = 5.0;

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
            Effect::Eq { gains_db: [0.0; 10] },
            Effect::Limiter {
                ceiling_db: -1.0,
                release_ms: 100.0,
            },
            Effect::Pitch { semitones: 2.0 },
            Effect::Repair,
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
            Effect::Eq { .. } => "EQ",
            Effect::Limiter { .. } => "Limiter",
            Effect::Pitch { .. } => "Pitch",
            Effect::Repair => "Repair",
        }
    }

    /// How long after a range the effect still sounds: until it is 60 dB below the sound that went in.
    pub fn tail(&self, rate: u32) -> Frames {
        match *self {
            Effect::Normalize { .. }
            | Effect::Loudness { .. }
            | Effect::HighPass { .. }
            | Effect::LowPass { .. }
            | Effect::Eq { .. }
            | Effect::Limiter { .. }
            | Effect::Pitch { .. }
            | Effect::Repair => 0,
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

    /// The part of a range from `start` to `end` that the effect hears: the range without its tail, which ends
    /// it, or reversed starts it. A range round the whole loop has no edges, so it hears all of it.
    pub fn input(&self, (start, end): (Frames, Frames), rate: u32, loop_end: Option<Frames>) -> (Frames, Frames) {
        let tail = self.tail(rate);
        if loop_end.is_some_and(|l| l > 0 && end - start >= l) {
            (start, end)
        } else if self.reversed() {
            (start.saturating_add(tail).min(end), end)
        } else {
            (start, end.saturating_sub(tail).max(start))
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

    /// How much sound before a range a filter hears, so it is settled where the range starts: its memory of the
    /// past fades to -80 dB in this time. The other effects start from silence at the range start.
    pub(crate) fn warmup(&self, rate: u32) -> Frames {
        if let Effect::Pitch { .. } | Effect::Repair = self {
            return self.lookahead(rate);
        }
        let stages = self.filter_stages(rate);
        let frames = stages.iter().map(Biquad::settle_frames).fold(0.0f64, f64::max);
        (frames.ceil() as Frames)
            .saturating_mul(stages.len() as Frames)
            .min(Frames::from(rate) * 2)
    }

    /// How much sound after a range the effect hears. The pitch shift works on windows of sound, and the last
    /// window of the range reaches past it; a repair learns from the sound on both sides.
    pub(crate) fn lookahead(&self, rate: u32) -> Frames {
        match self {
            Effect::Pitch { .. } => Frames::try_from(crate::pitch::window_len(rate)).unwrap_or(0),
            Effect::Repair => ms_frames(crate::repair::CONTEXT_MS, rate),
            _ => 0,
        }
    }

    /// The filter stages of high-pass, low-pass and EQ; none for the other effects.
    fn filter_stages(&self, rate: u32) -> Vec<Biquad> {
        // Butterworth: flat until the cutoff; the steep one is two stages.
        const GENTLE: &[f64] = &[std::f64::consts::FRAC_1_SQRT_2];
        const STEEP: &[f64] = &[0.541_196_1, 1.306_563];
        match *self {
            Effect::HighPass { hz, steep } | Effect::LowPass { hz, steep } => (if steep { STEEP } else { GENTLE })
                .iter()
                .map(|&q| Biquad::pass(*self, hz, q, rate))
                .collect(),
            // A band at 0 dB changes nothing, and one above what the rate can hold cannot be heard.
            Effect::Eq { gains_db } => EQ_BANDS_HZ
                .iter()
                .zip(gains_db)
                .filter(|&(&hz, db)| db != 0.0 && hz < rate as f32 * 0.45)
                .map(|(&hz, db)| Biquad::bell(hz, db, 1.0, rate))
                .collect(),
            _ => vec![],
        }
    }

    fn check(self) -> Result<Effect> {
        let within = |v: f32, r: std::ops::RangeInclusive<f32>| v.is_finite() && r.contains(&v);
        let problem = match self {
            Effect::Normalize { peak_db } if !within(peak_db, -60.0..=0.0) => {
                "normalize goes to a peak between -60 and 0 dB"
            }
            Effect::Loudness { lufs, .. } if !within(lufs, -60.0..=0.0) => {
                "the loudness must be between -60 and 0 LUFS"
            }
            // Above 0 dB only makes sense with a limiter after it, which brings the peaks back down.
            Effect::Loudness { peak_db, .. } if !within(peak_db, -60.0..=20.0) => {
                "the peak limit must be between -60 and +20 dB"
            }
            Effect::HighPass { hz, .. } | Effect::LowPass { hz, .. } if !within(hz, 10.0..=40_000.0) => {
                "a filter's frequency must be between 10 Hz and 40 kHz"
            }
            Effect::Eq { gains_db } if !gains_db.iter().all(|&db| within(db, -12.0..=12.0)) => {
                "each EQ band raises or lowers by at most 12 dB"
            }
            Effect::Limiter { ceiling_db, .. } if !within(ceiling_db, -40.0..=0.0) => {
                "a limiter's ceiling must be between -40 and 0 dB"
            }
            Effect::Limiter { release_ms, .. } if !within(release_ms, 1.0..=2000.0) => {
                "a limiter's release must be between 1 ms and 2 s"
            }
            Effect::Pitch { semitones } if !within(semitones, -12.0..=12.0) => {
                "the pitch moves by at most 12 semitones up or down"
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
    /// The whole effect, tail included: outside it the range changes nothing.
    pub start: Frames,
    pub end: Frames,
    /// Of the lane's kind.
    pub effect: Effect,
    /// How long the effect takes to come in and to go at the edges of what it hears, so it does not click.
    pub fade_in: Frames,
    pub fade_out: Frames,
}

/// The fade a new range starts with: short enough not to be heard as a fade, long enough not to click.
/// What normalize or loudness measured: its gain, and whether the peak limit held it back.
pub type Gain = (f32, bool);

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
pub(crate) fn tidy_lane(lane: &mut EffectLane, rate: u32, loop_end: Option<Frames>) -> Result<()> {
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
    let longest_repair = ms_frames(crate::repair::MAX_MS, rate);
    if lane
        .ranges
        .iter()
        .any(|r| matches!(r.effect, Effect::Repair) && r.end - r.start > longest_repair)
    {
        return Err(Error::Invalid(format!(
            "a repair can be at most {} ms long: split the clip and crossfade over a longer problem",
            crate::repair::MAX_MS
        )));
    }
    if lane.ranges.iter().any(|r| r.fade_in < 0 || r.fade_out < 0) {
        return Err(Error::Invalid("a fade cannot be shorter than nothing".into()));
    }
    let hears = |r: &EffectRange| r.effect.input((r.start, r.end), rate, loop_end);
    if lane.ranges.iter().any(|r| {
        let (from, to) = hears(r);
        to <= from
    }) {
        return Err(Error::Invalid(
            "the range is too short for the effect's tail: make the range longer or the tail shorter".into(),
        ));
    }
    // A range made shorter than its fades shortens both alike.
    for r in &mut lane.ranges {
        let (from, to) = hears(r);
        let (len, both) = (to - from, r.fade_in.saturating_add(r.fade_out));
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

/// Applies the lane to `sound`: the whole project from frame 0, with a loop one loop. Each range makes the
/// sound of its span from the sound that came into the lane, then all are copied in: a filter hears a little
/// before its range, which must be the lane's input, not what a range before it made. A lane costs what its
/// ranges cover. With a loop a range past the loop end goes on at the start.
/// Normalize and loudness ranges put what they measured into `gains`.
pub(crate) fn lane_into(
    lane: &EffectLane,
    project: &Project,
    sound: &mut [Vec<f32>; 2],
    gains: &mut HashMap<RangeId, Gain>,
) {
    if !lane.enabled {
        return;
    }
    let mut made = vec![];
    for range in &lane.ranges {
        let others: Vec<(Frames, Frames)> = lane
            .ranges
            .iter()
            .filter(|r| r.id != range.id)
            .map(|r| (r.start, r.end))
            .collect();
        let (range_sound, gain) = range_sound(range, &others, project, sound);
        if let Some(gain) = gain {
            gains.insert(range.id, gain);
        }
        made.push((range.start, range_sound));
    }
    let loop_end = project.loop_end.filter(|l| *l > 0);
    for (from, made) in made {
        for (channel, made) in sound.iter_mut().zip(&made) {
            write(channel, loop_end, from, made);
        }
    }
}

/// Copies `made` into `channel` from timeline frame `from`; with a loop round it, otherwise what lies outside
/// is left out.
fn write(channel: &mut [f32], loop_end: Option<Frames>, from: Frames, mut made: &[f32]) {
    let mut at = from;
    while !made.is_empty() {
        let pos = loop_end.map_or(at, |l| at.rem_euclid(l));
        let to_loop_end = loop_end.map_or(usize::MAX, |l| usize::try_from(l - pos).unwrap_or(0).max(1));
        let (piece, rest) = made.split_at(made.len().min(to_loop_end));
        let skip = usize::try_from(pos.saturating_neg()).unwrap_or(0);
        let first = usize::try_from(pos).unwrap_or(0);
        for (to, from) in channel.iter_mut().skip(first).zip(piece.iter().skip(skip)) {
            *to = *from;
        }
        at = at.saturating_add(Frames::try_from(piece.len()).unwrap_or(Frames::MAX));
        made = rest;
    }
}

/// The sound of one range's span, from the range start, made from the sound that came into the lane; for
/// normalize and loudness also what they measured.
fn range_sound(
    range: &EffectRange,
    others: &[(Frames, Frames)],
    project: &Project,
    sound: &[Vec<f32>; 2],
) -> ([Vec<f32>; 2], Option<Gain>) {
    let rate = project.sample_rate;
    let loop_end = project.loop_end.filter(|l| *l > 0);
    let effect = range.effect;
    // A range round the whole loop has no edges: its end runs straight into its start.
    let whole_loop = loop_end.filter(|l| range.end - range.start >= *l);
    let (start, end) = effect.input((range.start, range.end), rate, loop_end);
    let (fade_in, fade_out) = (range.fade_in.max(1) as f32, range.fade_out.max(1) as f32);
    let weight = |t: Frames| {
        let (into, left) = ((t - start) as f32, (end - t) as f32);
        match (t >= start && t < end, whole_loop.is_some()) {
            // Round the whole loop, before and after the range is the loop again.
            (_, true) => 1.0,
            (false, false) => 0.0,
            (true, false) => ((into + 1.0) / fade_in).min(left / fade_out).min(1.0),
        }
    };
    // A filter first hears a little before the range, so it is settled where the range starts.
    let before = effect.warmup(rate);
    let after = usize::try_from(effect.lookahead(rate)).unwrap_or(0);
    let from = range.start - before;
    let len = usize::try_from(range.end - from).unwrap_or(0);
    let mut buffer = read(sound, loop_end, from, len + after);
    let mut weights: Vec<f32> = (from..range.end.saturating_add(after as Frames)).map(weight).collect();
    let gain = match effect {
        Effect::Normalize { .. } | Effect::Loudness { .. } => {
            let heard = usize::try_from(start - from).unwrap_or(0)..usize::try_from(end - from).unwrap_or(0);
            let heard = buffer.each_ref().map(|c| c.get(heard.clone()).unwrap_or(&[]));
            Some(measured_gain(&effect, heard, rate, start))
        }
        _ => None,
    };
    // Round the whole loop the tail goes on into the range's own start, so it is made into silence after
    // the loop and added there.
    let tail = whole_loop.map_or(0, |_| usize::try_from(effect.tail(rate)).unwrap_or(0));
    if effect.reversed() {
        buffer.iter_mut().chain([&mut weights]).for_each(|c| {
            c.splice(0..0, std::iter::repeat_n(0.0, tail));
            c.reverse();
        });
    } else {
        buffer
            .iter_mut()
            .chain([&mut weights])
            .for_each(|c| c.resize(c.len() + tail, 0.0));
    }
    if let Effect::Repair = effect {
        // The sound under the lane's other repairs is what they replace: no use to learn from.
        // Only the ranges inside what this one reads; with a loop also those one loop away.
        let to = from.saturating_add(Frames::try_from(weights.len()).unwrap_or(Frames::MAX));
        let shifts = loop_end.map_or(vec![0], |l| vec![-l, 0, l]);
        let near: Vec<(Frames, Frames)> = others
            .iter()
            .copied()
            .filter(|&(s, e)| {
                let shorter_loop = loop_end.is_some_and(|l| to - from > l);
                shorter_loop || shifts.iter().any(|d| s + d < to && e + d > from)
            })
            .collect();
        let covered = |t: Frames| {
            near.iter().any(|&(s, e)| {
                let t = loop_end.map_or(t, |l| s + (t - s).rem_euclid(l));
                (s..e).contains(&t)
            })
        };
        let skip: Vec<bool> = if near.is_empty() {
            vec![]
        } else {
            (from..to).map(covered).collect()
        };
        crate::repair::fill(&mut buffer, &weights, &skip);
    } else {
        apply(effect, rate, gain.map_or(1.0, |g| g.0), &mut buffer, &weights);
    }
    if whole_loop.is_some() {
        seam_fade(&mut buffer, usize::try_from(before).unwrap_or(0), len, after / 4);
    }
    buffer.iter_mut().for_each(|c| c.truncate(len.saturating_add(tail)));
    if effect.reversed() {
        buffer.iter_mut().for_each(|c| c.reverse());
    }
    let before = usize::try_from(before).unwrap_or(0);
    buffer.iter_mut().for_each(|c| drop(c.drain(..before.min(c.len()))));
    if let Some(l) = whole_loop {
        let first = if effect.reversed() { -(tail as Frames) } else { 0 };
        buffer = buffer.map(|c| {
            let mut folded = vec![0.0; usize::try_from(l).unwrap_or(0)];
            for (t, v) in (first..).zip(c) {
                if let Some(f) = usize::try_from(t.rem_euclid(l)).ok().and_then(|i| folded.get_mut(i)) {
                    *f += v;
                }
            }
            folded
        });
    }
    (buffer, gain)
}

/// Crossfades what an effect made past the loop end into the loop start over `fade` frames. An effect that
/// builds each moment on the one before, like the pitch shift, does not come out the same at both ends.
fn seam_fade(buffer: &mut [Vec<f32>; 2], start: usize, end: usize, fade: usize) {
    for channel in buffer {
        for j in 0..fade {
            let (Some(&past), Some(&at)) = (channel.get(end + j), channel.get(start + j)) else {
                break;
            };
            let t = (j as f32 + 0.5) / fade as f32;
            if let Some(s) = channel.get_mut(start + j) {
                *s = past + t * (at - past);
            }
        }
    }
}

/// The gain normalize or loudness gives `audio`, the sound under a range that starts at timeline frame `start`.
fn measured_gain(effect: &Effect, audio: [&[f32]; 2], rate: u32, start: Frames) -> Gain {
    let peak = audio.into_iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    let lufs = match effect {
        Effect::Loudness { .. } => from_slices(slices(audio, rate, start).iter()),
        _ => None,
    };
    gain_for(effect, peak, lufs)
}

/// Runs the effect over `sound` in place. Every effect gets the same: both sides as flat arrays, and how much
/// of the effect each frame gets, so each can be made as fast as it likes on its own.
fn apply(effect: Effect, rate: u32, gain: f32, sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    match effect {
        Effect::Normalize { .. } | Effect::Loudness { .. } => apply_gain(gain, sound, weights),
        Effect::HighPass { .. } | Effect::LowPass { .. } | Effect::Eq { .. } => {
            filter(&effect.filter_stages(rate), sound, weights);
        }
        Effect::Limiter { ceiling_db, release_ms } => limiter(
            db_to_amp(ceiling_db),
            usize::try_from(ms_frames(LOOKAHEAD_MS, rate)).unwrap_or(1),
            1.0 - (-1.0 / ms_frames(release_ms, rate) as f32).exp(),
            sound,
            weights,
        ),
        Effect::Echo {
            delay_ms,
            fade_db,
            level_db,
            ..
        } => echo(
            ms_frames(delay_ms, rate),
            db_to_amp(-fade_db),
            db_to_amp(level_db),
            sound,
            weights,
        ),
        Effect::Reverb { seconds, level_db, .. } => {
            Reverb::new(seconds, rate).run(sound, weights, db_to_amp(level_db));
        }
        Effect::Pitch { semitones } => crate::pitch::shift(semitones, rate, sound, weights),
        // Needs the lane's other ranges: made in range_sound.
        Effect::Repair => {}
    }
}

fn apply_gain(gain: f32, sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    for channel in sound {
        for (s, weight) in channel.iter_mut().zip(weights) {
            *s += weight * (*s * gain - *s);
        }
    }
}

fn filter(stages: &[Biquad], sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    for channel in sound {
        let mut memory = vec![[0.0; 4]; stages.len()];
        for (s, weight) in channel.iter_mut().zip(weights) {
            let dry = *s;
            let mut wet = dry;
            for (stage, memory) in stages.iter().zip(&mut memory) {
                wet = stage.step(memory, f64::from(wet)) as f32;
            }
            *s = dry + weight * (wet - dry);
        }
    }
}

/// Turns both sides down together, so the sound stays in place, by the gain each frame needs to stay at or below
/// `ceiling`. The whole sound is there, so it can see a peak `lookahead` frames before it comes.
fn limiter(ceiling: f32, lookahead: usize, release: f32, sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    let [left, right] = &*sound;
    let needed: Vec<f32> = left
        .iter()
        .zip(right)
        .map(|(l, r)| {
            let peak = l.abs().max(r.abs());
            if peak > ceiling { ceiling / peak } else { 1.0 }
        })
        .collect();
    // The lowest need of the next `lookahead` frames, averaged over the last `lookahead`: every frame of that
    // average holds the need of the peak, so it is low enough there, and it slopes down smoothly before it.
    let lowest = lowest_ahead(&needed, lookahead.max(1));
    let mut sum = 0.0f64;
    let mut gain = 1.0f32;
    let gains: Vec<f32> = (0..lowest.len())
        .map(|i| {
            sum += f64::from(lowest.get(i).copied().unwrap_or(1.0));
            if let Some(gone) = i.checked_sub(lookahead).and_then(|j| lowest.get(j)) {
                sum -= f64::from(*gone);
            }
            let smooth = (sum / (i + 1).min(lookahead) as f64) as f32;
            // Down at once, up slowly: coming back fast would be heard as pumping.
            gain = smooth.min(gain + (1.0 - gain) * release);
            gain
        })
        .collect();
    for channel in sound {
        for ((s, weight), gain) in channel.iter_mut().zip(weights).zip(&gains) {
            *s += weight * (*s * gain - *s);
        }
    }
}

/// For each frame, the lowest of it and the `n - 1` after it.
fn lowest_ahead(values: &[f32], n: usize) -> Vec<f32> {
    let mut out = vec![1.0; values.len()];
    // Frames still to come, from the lowest; a later one that is lower makes the ones before it irrelevant.
    let mut window: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for i in (0..values.len()).rev() {
        let value = values.get(i).copied().unwrap_or(1.0);
        while window.back().and_then(|&j| values.get(j)).is_some_and(|v| *v >= value) {
            window.pop_back();
        }
        window.push_back(i);
        while window.front().is_some_and(|&j| j >= i + n) {
            window.pop_front();
        }
        if let (Some(o), Some(v)) = (out.get_mut(i), window.front().and_then(|&j| values.get(j))) {
            *o = *v;
        }
    }
    out
}

/// Repeats every `delay` frames, `feedback` as loud as the one before, the first at `level`.
fn echo(delay: Frames, feedback: f32, level: f32, sound: &mut [Vec<f32>; 2], weights: &[f32]) {
    for channel in sound {
        let mut line = vec![0.0f32; usize::try_from(delay).unwrap_or(0)];
        let len = line.len().max(1);
        let mut at = 0;
        for (s, weight) in channel.iter_mut().zip(weights) {
            if let Some(slot) = line.get_mut(at) {
                let delayed = *slot;
                *slot = *s * weight + feedback * delayed;
                *s += level * delayed;
            }
            at = next(at, len);
        }
    }
}

/// The frequency as an angle per frame, kept below the highest frequency the rate can hold.
fn angle(hz: f32, rate: u32) -> f64 {
    std::f64::consts::TAU * f64::from(cutoff(hz, rate)) / f64::from(rate.max(1))
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
    /// A high-pass or low-pass stage.
    fn pass(effect: Effect, hz: f32, q: f64, rate: u32) -> Biquad {
        let (sin, cos) = angle(hz, rate).sin_cos();
        let alpha = sin / (2.0 * q);
        let b = match effect {
            Effect::HighPass { .. } => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            _ => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
        };
        Biquad::normalized(b, [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    }

    /// Raises or lowers the sound around `hz` by `gain_db`, in a bell `octaves` wide.
    fn bell(hz: f32, gain_db: f32, octaves: f32, rate: u32) -> Biquad {
        let w0 = angle(hz, rate);
        let (sin, cos) = w0.sin_cos();
        let a = 10f64.powf(f64::from(gain_db) / 40.0);
        let alpha = sin * (std::f64::consts::LN_2 / 2.0 * f64::from(octaves) * w0 / sin).sinh();
        Biquad::normalized(
            [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        )
    }

    fn normalized(b: [f64; 3], [a0, a1, a2]: [f64; 3]) -> Biquad {
        Biquad {
            b: b.map(|v| v / a0),
            a: [a1 / a0, a2 / a0],
        }
    }

    /// How many frames its memory of the past takes to fade to -80 dB: set by its slowest pole.
    fn settle_frames(&self) -> f64 {
        let [a1, a2] = self.a;
        let disc = a1 * a1 - 4.0 * a2;
        let radius = if disc < 0.0 {
            a2.max(0.0).sqrt()
        } else {
            ((-a1).abs() + disc.sqrt()) / 2.0
        };
        if radius >= 1.0 {
            f64::MAX
        } else {
            9.21 / -radius.max(1e-12).ln()
        }
    }

    /// One frame through the filter, going on from the last two inputs and outputs in `memory`.
    #[inline]
    pub(crate) fn step(&self, memory: &mut [f64; 4], x: f64) -> f64 {
        let [x1, x2, y1, y2] = *memory;
        let y = self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
        *memory = [x, x1, y, y1];
        y
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

    /// Adds the room to `sound`, fed with both sides of it times `weights`.
    fn run(&mut self, sound: &mut [Vec<f32>; 2], weights: &[f32], level: f32) {
        let [left, right] = &*sound;
        let feed: Vec<f32> = left
            .iter()
            .zip(right)
            .zip(weights)
            .map(|((l, r), w)| (l + r) * 0.5 * w * self.scale)
            .collect();
        for ((channel, combs), passes) in sound.iter_mut().zip(&mut self.combs).zip(&mut self.passes) {
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

/// The gain for normalize and loudness from the loudest sample and, for loudness, the loudness of the sound
/// under the range; and whether the peak limit held it back. Silence is left alone.
fn gain_for(effect: &Effect, peak: f32, lufs: Option<f64>) -> Gain {
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
    use crate::{Command, Session, Source, TrackId, render};

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
    fn an_eq_band_raises_its_octave_and_leaves_the_others() {
        let mut gains_db = [0.0; 10];
        gains_db[5] = 6.0;
        let db = |hz| 20.0 * (level_after(&tone_session(hz, Effect::Eq { gains_db })) / 0.5).log10();
        // The 1 kHz band; two octaves away hardly anything.
        assert!((db(1000.0) - 6.0).abs() < 0.1, "{}", db(1000.0));
        assert!(db(4000.0).abs() < 0.3, "{}", db(4000.0));
        assert!(db(250.0).abs() < 0.3, "{}", db(250.0));
    }

    #[test]
    fn a_limiter_after_loudness_reaches_the_target_without_passing_the_ceiling() {
        // The tone peaks at -6 dB and reads -6 LUFS, so 0 LUFS takes its peaks to 0 dB; the limiter holds them at -3.
        let mut s = tone_session(
            997.0,
            Effect::Loudness {
                lufs: 0.0,
                peak_db: 20.0,
            },
        );
        let track = s.project().tracks[0].id;
        s.execute(Command::AddEffectLane {
            track: Some(track),
            effect: Effect::Limiter {
                ceiling_db: -3.0,
                release_ms: 50.0,
            },
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[1].id;
        s.execute(Command::AddEffectRange {
            lane,
            start: 0,
            end: 48_000,
        })
        .unwrap();
        let sound = render(s.project(), 0, 48_000);
        let peak = sound.iter().flatten().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= db_to_amp(-3.0) + 1e-6, "{peak}");
        let lufs = loudness(&render(s.project(), 12_000, 24_000), 48_000).unwrap();
        // A steady tone held 3 dB down reads 3 dB quieter: turned down smoothly, not cut off at the ceiling.
        assert!((lufs + 3.0).abs() < 0.1, "{lufs}");
    }

    #[test]
    fn a_limiter_turns_down_just_around_a_spike() {
        // Quiet sound with one spike at frame 1000; 5 frames of lookahead, back up within about 10 frames.
        let mut sound: [Vec<f32>; 2] = [vec![0.1; 2000], vec![0.1; 2000]];
        sound[0][1000] = 1.0;
        let weights = vec![1.0; 2000];
        limiter(0.5, 5, 0.1, &mut sound, &weights);
        assert!((sound[0][1000] - 0.5).abs() < 1e-6, "the spike at the ceiling");
        assert!(sound[1][1000] < 0.06, "the other side turned down with it");
        assert!(
            sound[0][996] < 0.1 && sound[0][990] == 0.1,
            "down shortly before, not long before"
        );
        assert!((sound[0][1100] - 0.1).abs() < 1e-4, "back up after the release");
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

    /// One click at frame 100 on a track, with a lane of `effect` that hears 0..200; its tail is added on.
    #[test]
    fn pitch_moves_a_tone_and_keeps_its_level_and_timing() {
        for (semitones, hz) in [(12.0, 880), (-7.0, 294)] {
            let s = tone_session(440.0, Effect::Pitch { semitones });
            let [left, _] = render(s.project(), 12_000, 24_000);
            // Half a second: two upward zero crossings per cycle per second.
            let crossings = left.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count() as i32 * 2;
            assert!((crossings - hz).abs() <= 2, "{semitones} st: {crossings} Hz");
            assert!((level_after(&s) - 0.5).abs() < 0.01, "{}", level_after(&s));
        }
        // A tone starting in the middle still starts there; the shift only softens it by a few ms.
        let mut s = tone_session(300.0, Effect::Pitch { semitones: 5.0 });
        let clip = s.project().tracks[0].clips[0].id;
        s.execute(Command::TrimClip {
            clip,
            start: 24_000,
            end: 48_000,
        })
        .unwrap();
        let [left, _] = render(s.project(), 0, 48_000);
        let first = left.iter().position(|v| v.abs() > 0.05).unwrap();
        let full = left.iter().position(|v| v.abs() > 0.4).unwrap();
        assert!(
            (23_500..24_000).contains(&first) && (24_000..24_500).contains(&full),
            "{first} {full}"
        );
    }

    /// The shift builds each moment on the one before, so round a whole loop its ends did not meet: a click.
    #[test]
    fn a_pitch_range_round_the_whole_loop_has_no_seam() {
        let mut s = tone_session(440.0, Effect::Pitch { semitones: 3.0 });
        s.execute(Command::SetLoopEnd { at: Some(48_000) }).unwrap();
        let [left, _] = render(s.project(), 47_000, 2_000);
        let step = left.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // The steepest a 0.5 sine at the shifted pitch gets in one frame.
        let clean = 0.5 * 440.0 * 2f32.powf(0.25) * std::f32::consts::TAU / 48_000.0;
        assert!(step < clean * 1.05, "{step} against {clean}");
    }

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
        let tail = effect.tail(RATE);
        let (start, end) = if effect.reversed() {
            (-tail, 200)
        } else {
            (0, 200 + tail)
        };
        s.execute(Command::AddEffectRange { lane, start, end }).unwrap();
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

    #[test]
    fn new_settings_keep_the_range_edges_and_a_tail_longer_than_the_range_is_refused() {
        let reverb = |seconds| Effect::Reverb {
            seconds,
            level_db: 0.0,
            reversed: false,
        };
        let mut s = click_session(reverb(0.5));
        let edges = |s: &Session| {
            let r = &s.project().tracks[0].effects[0].ranges[0];
            (r.start, r.end, r.effect.input((r.start, r.end), RATE, None))
        };
        let (start, end, _) = edges(&s);
        let range = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::SetEffect {
            range,
            effect: reverb(0.3),
        })
        .unwrap();
        let tail = reverb(0.3).tail(RATE);
        assert_eq!(
            edges(&s),
            (start, end, (start, end - tail)),
            "it hears more, the edges stay"
        );
        assert!(
            s.execute(Command::SetEffect {
                range,
                effect: reverb(5.0),
            })
            .is_err()
        );
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
            start: 1300,
            end: 2400,
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
            start: 2500,
            end: 3600,
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

        // The tone is at -6 dB, so 0 LUFS needs +6 dB, which would take the peak to 0 dB.
        let s = tone_session(
            997.0,
            Effect::Loudness {
                lufs: 0.0,
                peak_db: -1.0,
            },
        );
        assert!(lufs_of(&s) < -0.5, "held back: {}", lufs_of(&s));
        let peak = render(s.project(), 0, 48_000)[0]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - db_to_amp(-1.0)).abs() < 1e-3, "{peak}");
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
        // The tail before what it hears, 600..800, which the loop wraps round to 0..200 with the click.
        let range = s.project().tracks[0].effects[0].ranges[0].id;
        s.execute(Command::SetEffectRange {
            range,
            start: 240,
            end: 800,
        })
        .unwrap();
        s.execute(Command::SetLoopEnd { at: Some(600) }).unwrap();
        let [left, _] = render(s.project(), 0, 600);
        let at = |i: usize, db: f32| (left[i] - db_to_amp(db)).abs() < 1e-4;
        assert!(at(40, -6.0) && at(580, -18.0) && at(520, -30.0));
    }

    /// Bursts of clicks over `clicks` in `clean`, each repaired by a range of one Repair lane.
    fn repaired(clean: &[f32], clicks: &[(Frames, Frames)]) -> Vec<f32> {
        let mut dirty = clean.to_vec();
        for &(start, end) in clicks {
            for (i, s) in dirty.iter_mut().enumerate().take(end as usize).skip(start as usize) {
                *s = if i % 2 == 0 { 0.9 } else { -0.9 };
            }
        }
        let mut s = Session::default();
        s.execute(Command::AddTrack { name: "t".into() }).unwrap();
        let track = s.project().tracks[0].id;
        let source = Arc::new(Source {
            path: None,
            name: "clicks".into(),
            sample_rate: 48_000,
            channels: vec![dirty],
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
            effect: Effect::Repair,
        })
        .unwrap();
        let lane = s.project().tracks[0].effects[0].id;
        for &(start, end) in clicks {
            s.execute(Command::AddEffectRange { lane, start, end }).unwrap();
        }
        render(s.project(), 0, 48_000)[0].clone()
    }

    fn chord() -> Vec<f32> {
        (0..48_000)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                [220.0f32, 330.0, 523.0]
                    .iter()
                    .map(|hz| 0.2 * (t * hz * std::f32::consts::TAU).sin())
                    .sum()
            })
            .collect()
    }

    #[test]
    fn repair_fills_a_click_in_a_chord_and_keeps_the_level_of_noise() {
        let chord = chord();
        let out = repaired(&chord, &[(24_000, 24_480)]);
        let worst = out
            .iter()
            .zip(&chord)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.02, "{worst}");

        let mut state = 7;
        let noise: Vec<f32> = (0..48_000)
            .map(|_| 0.5 * crate::repair::random(&mut state) as f32)
            .collect();
        let out = repaired(&noise, &[(24_000, 24_480)]);
        assert_eq!(out[..24_000], noise[..24_000]);
        assert_eq!(out[24_480..], noise[24_480..]);
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let db = 20.0 * (rms(&out[24_000..24_480]) / rms(&noise[24_000..24_480])).log10();
        assert!((-3.0..1.0).contains(&db), "{db}");
    }

    /// A repair learned the other clicks near it as if they were the sound, and one 1 ms away rang louder
    /// than the click.
    #[test]
    fn repairs_near_each_other_do_not_learn_from_the_other_clicks() {
        let chord = chord();
        for clicks in [
            [(23_000, 23_096), (24_000, 24_096)],
            [(23_904, 23_952), (24_000, 24_096)],
        ] {
            let out = repaired(&chord, &clicks);
            let worst = out
                .iter()
                .zip(&chord)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(worst < 0.01, "{clicks:?}: {worst}");
        }
    }
}
