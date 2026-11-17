//! The menu of an effect lane: the settings of one range or of all, fades, order, removing; and the FX menu.

use egui::Ui;
use refrain_engine::{Command, Effect, EffectRange, Frames, LaneId, RangeId, TrackId};

use crate::Editor;

impl Editor {
    pub(crate) fn lane_menu_ui(&mut self, ui: &mut Ui, id: LaneId, range: Option<RangeId>) {
        let project = self.session.project().clone();
        let Some(lane) = project.lane(id) else {
            ui.close();
            return;
        };
        let this_range = range.and_then(|id| lane.range(id));
        ui.label(match this_range {
            Some(_) => format!("{}: this range", lane.effect.name()),
            None => format!("{}: all ranges", lane.effect.name()),
        });
        // While a value is dragged it lives here; the project only changes when it is let go, as one undo step.
        let mut effect = match self.effect_edit {
            Some((lane, edited, effect)) if lane == id && edited == range => effect,
            _ => this_range.map_or(lane.effect, |r| r.effect),
        };
        let (changed, done) = effect_settings(ui, &mut effect);
        if changed {
            self.effect_edit = Some((id, range, effect));
        }
        if done && let Some((lane, edited, effect)) = self.effect_edit.take() {
            match edited {
                Some(range) => self.run(Command::SetEffect { range, effect }),
                None => self.run(Command::SetLaneEffect { lane, effect }),
            };
        }
        if let Some(r) = this_range {
            self.fade_rows(ui, r, project.sample_rate);
        }
        if let Some(r) = this_range
            && lane.ranges.len() > 1
            && ui.button("Use these settings for all ranges in this lane").clicked()
        {
            self.run(Command::SetLaneEffect {
                lane: id,
                effect: r.effect,
            });
            ui.close();
        }
        ui.separator();
        if let Some(range) = range
            && ui.button("Remove this range").clicked()
        {
            self.run(Command::RemoveEffectRange { range });
            ui.close();
        }
        let lanes = project
            .tracks
            .iter()
            .map(|t| &t.effects)
            .chain(std::iter::once(&project.mix_effects))
            .find(|l| l.iter().any(|l| l.id == id));
        let index = lanes.and_then(|l| l.iter().position(|l| l.id == id)).unwrap_or(0);
        let count = lanes.map_or(0, Vec::len);
        if ui
            .add_enabled(index > 0, egui::Button::new("Move up (applied earlier)"))
            .clicked()
        {
            self.run(Command::MoveEffectLane { lane: id, down: false });
            ui.close();
        }
        if ui
            .add_enabled(index + 1 < count, egui::Button::new("Move down (applied later)"))
            .clicked()
        {
            self.run(Command::MoveEffectLane { lane: id, down: true });
            ui.close();
        }
        if ui.button("Remove this effect").clicked() {
            self.run(Command::RemoveEffectLane { lane: id });
            ui.close();
        }
    }

    /// How long the range's effect takes to come in and to go, in ms; together at most the range.
    fn fade_rows(&mut self, ui: &mut Ui, r: &EffectRange, rate: u32) {
        let per_ms = f64::from(rate.max(1)) / 1000.0;
        let (fade_in, fade_out) = self.shown_fades(r);
        let len = r.end - r.start;
        let ms = |frames: Frames| (frames as f64 / per_ms) as f32;
        let (mut in_ms, mut out_ms) = (ms(fade_in), ms(fade_out));
        let speed = (f64::from(ms(len)) / 400.0).max(0.1);
        let rows = [
            ("Fade in", &mut in_ms, 0.0..=ms(len - fade_out), speed, " ms", 0),
            ("Fade out", &mut out_ms, 0.0..=ms(len - fade_in), speed, " ms", 0),
        ];
        let (changed, done) = number_rows(ui, rows);
        if changed {
            let frames = |ms: f32| ((f64::from(ms) * per_ms).round() as Frames).clamp(0, len);
            let fade_in = frames(in_ms);
            self.fade_edit = Some((r.id, fade_in, frames(out_ms).min(len - fade_in)));
        }
        if done
            && let Some((range, fade_in, fade_out)) = self.fade_edit.take()
            && (fade_in, fade_out) != (r.fade_in, r.fade_out)
        {
            self.run(Command::SetRangeFades {
                range,
                fade_in,
                fade_out,
            });
        }
    }

    /// The effects a track or the mix (`None`) can get, and folding a track's lanes.
    pub(crate) fn effects_menu_ui(&mut self, ui: &mut Ui, track: Option<TrackId>) {
        ui.label("Add effect");
        for effect in Effect::defaults() {
            if ui.button(effect.name()).clicked() {
                self.run(Command::AddEffectLane { track, effect });
                if let Some(track) = track {
                    self.folded.remove(&track);
                }
                ui.close();
            }
        }
        let Some(track) = track else { return };
        let has_lanes = self
            .session
            .project()
            .track(track)
            .is_some_and(|t| !t.effects.is_empty());
        if has_lanes {
            ui.separator();
            let folded = self.folded.contains(&track);
            if ui
                .button(if folded {
                    "Show effect lanes"
                } else {
                    "Hide effect lanes"
                })
                .clicked()
            {
                if folded {
                    self.folded.remove(&track);
                } else {
                    self.folded.insert(track);
                }
                ui.close();
            }
        }
    }
}

pub(crate) type NumberRow<'a> = (
    &'static str,
    &'a mut f32,
    std::ops::RangeInclusive<f32>,
    f64,
    &'static str,
    usize,
);

/// One line per number: label and drag field. Returns whether one changed, and whether that change is finished.
pub(crate) fn number_rows<const N: usize>(ui: &mut Ui, rows: [NumberRow<'_>; N]) -> (bool, bool) {
    let (mut changed, mut done) = (false, false);
    for (label, value, range, speed, suffix, decimals) in rows {
        let response = ui
            .horizontal(|ui| {
                // Tied to its label, so a screen reader names the field.
                let label = ui.label(label);
                ui.add(
                    egui::DragValue::new(value)
                        .range(range)
                        .speed(speed)
                        .fixed_decimals(decimals)
                        .suffix(suffix),
                )
                .labelled_by(label.id)
            })
            .inner;
        changed |= response.changed();
        done |= response.drag_stopped() || response.lost_focus() || (response.changed() && !response.dragged());
    }
    (changed, done)
}

/// Returns whether it was switched.
fn reversed_box(ui: &mut Ui, reversed: &mut bool, hint: &str) -> bool {
    ui.checkbox(reversed, "Reversed").on_hover_text(hint).changed()
}

/// The settings of one effect. Returns whether a value changed, and whether the change is finished, which is
/// at once for a choice and on release for a dragged number.
fn effect_settings(ui: &mut Ui, effect: &mut Effect) -> (bool, bool) {
    match effect {
        Effect::Normalize { peak_db } => number_rows(ui, [("Loudest point at", peak_db, -60.0..=0.0, 0.1, " dB", 1)]),
        Effect::Loudness { lufs, peak_db } => {
            let rows = [
                ("As loud as", lufs, -60.0..=0.0, 0.1, " LUFS", 1),
                ("Loudest point at most", peak_db, -60.0..=0.0, 0.1, " dB", 1),
            ];
            number_rows(ui, rows)
        }
        Effect::HighPass { hz, steep } | Effect::LowPass { hz, steep } => {
            // Each pixel dragged moves the frequency by 1 %, so low and high frequencies are equally easy to set.
            let speed = f64::from(hz.max(10.0) * 0.01);
            let (mut changed, mut done) = number_rows(ui, [("Frequency", hz, 10.0..=20_000.0, speed, " Hz", 0)]);
            ui.horizontal(|ui| {
                ui.label("Slope");
                for (value, text, hint) in [
                    (false, "12 dB/octave", "Gentle: sounds natural"),
                    (true, "24 dB/octave", "Steep: removes more right past the frequency"),
                ] {
                    if ui.radio_value(steep, value, text).on_hover_text(hint).changed() {
                        (changed, done) = (true, true);
                    }
                }
            });
            (changed, done)
        }
        Effect::Echo {
            delay_ms,
            fade_db,
            level_db,
            reversed,
        } => {
            let rows = [
                ("Repeat every", delay_ms, 10.0..=2000.0, 1.0, " ms", 0),
                ("First echo at", level_db, -40.0..=0.0, 0.1, " dB", 1),
                ("Each next one", fade_db, 3.0..=40.0, 0.1, " dB quieter", 1),
            ];
            let (changed, done) = number_rows(ui, rows);
            let flipped = reversed_box(
                ui,
                reversed,
                "The echoes come before the sound, getting louder towards it",
            );
            (changed || flipped, done || flipped)
        }
        Effect::Reverb {
            seconds,
            level_db,
            reversed,
        } => {
            let rows = [
                ("Dies away in", seconds, 0.1..=10.0, 0.01, " s", 2),
                ("Volume", level_db, -40.0..=0.0, 0.1, " dB", 1),
            ];
            let (changed, done) = number_rows(ui, rows);
            let flipped = reversed_box(ui, reversed, "The room comes before the sound and swells into it");
            (changed || flipped, done || flipped)
        }
    }
}
