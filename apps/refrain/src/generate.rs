//! Generated clips: adding one, and the panel with its settings.

use egui::{Align2, Order, Ui, vec2};
use refrain_engine::{ClipId, Command, Frames, Generator, NoiseColor, Wave, color_name, played_hz, wave_name};

use crate::Editor;
use crate::effect_menu::number_rows;

/// The settings of one generated clip.
pub(crate) struct Panel {
    clip: ClipId,
    generator: Generator,
    /// While a value is dragged, the project only changes when it is let go, as one undo step.
    dragging: bool,
}

impl Panel {
    pub(crate) fn new(clip: ClipId, generator: Generator) -> Panel {
        Panel {
            clip,
            generator,
            dragging: false,
        }
    }
}

/// Seconds a new generated clip lasts without a loop; with one it lasts one loop.
const NEW_SECONDS: f64 = 10.0;

impl Editor {
    /// Adds a clip of the generator last set, or a tone, on a new track at the play position, and opens its
    /// settings. Its length is changed by dragging its edges.
    pub(crate) fn add_generator(&mut self) {
        let project = self.session.project();
        let rate = if project.sample_rate == 0 {
            48_000
        } else {
            project.sample_rate
        };
        let at = self.player.position();
        let (start, len) = match project.loop_end {
            // A clip starts inside the loop; one loop long it repeats without a seam.
            Some(l) => (if at < l { at } else { 0 }, l),
            None => (at, (NEW_SECONDS * f64::from(rate)).round() as Frames),
        };
        let generator = self.last_generator;
        if self.run(Command::AddGeneratedClip {
            track: None,
            generator,
            start,
            len,
        }) {
            let added = self.session.project().tracks.last().and_then(|t| t.clips.first());
            self.selected = added.map(|c| c.id);
            self.generate = self.selected.map(|clip| Panel::new(clip, generator));
        }
    }

    pub(crate) fn generate_panel(&mut self, ctx: &egui::Context) {
        let Some(mut panel) = self.generate.take() else { return };
        // Undo or a delete may have changed or removed the clip.
        let generated = self.session.project().clip(panel.clip);
        let Some(generated) = generated.and_then(|(_, c)| c.source.generated.clone()) else {
            return;
        };
        if !panel.dragging {
            panel.generator = generated.generator;
        }
        let mut open = true;
        egui::Window::new("Generator")
            .open(&mut open)
            .order(Order::Foreground)
            .pivot(Align2::RIGHT_TOP)
            .default_pos(ctx.content_rect().right_top() + vec2(-16.0, 70.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| self.generate_ui(ui, &mut panel));
        if open {
            self.generate = Some(panel);
        }
    }

    fn generate_ui(&mut self, ui: &mut Ui, panel: &mut Panel) {
        let (mut changed, mut done) = (false, false);
        ui.horizontal(|ui| {
            for kind in Generator::defaults() {
                let same = std::mem::discriminant(&kind) == std::mem::discriminant(&panel.generator);
                if ui.selectable_label(same, kind.kind_name()).clicked() && !same {
                    panel.generator = kind;
                    (changed, done) = (true, true);
                }
            }
        });
        ui.separator();
        let (c, d) = generator_settings(ui, &mut panel.generator);
        (changed, done) = (changed || c, done || d);
        let project = self.session.project();
        if let Some(l) = project.loop_end {
            ui.weak(loop_note(panel.generator, project.sample_rate, l));
        }
        ui.weak("Drag the clip's edges to change its length.");
        panel.dragging |= changed;
        if done {
            panel.dragging = false;
            let generator = panel.generator;
            if self.run(Command::SetGenerator {
                clip: panel.clip,
                generator,
            }) {
                self.last_generator = generator;
            }
        }
    }
}

/// How the sound was fitted so the loop has no click.
fn loop_note(generator: Generator, rate: u32, loop_end: Frames) -> String {
    match generator {
        Generator::Tone { hz, .. } => format!(
            "Plays at {:.2} Hz: a whole number of waves per loop",
            played_hz(hz, rate, Some(loop_end))
        ),
        Generator::Blips { every_ms, .. } => {
            let loop_ms = loop_end as f64 * 1000.0 / f64::from(rate.max(1));
            let count = (loop_ms / f64::from(every_ms)).round().max(1.0);
            format!("One every {:.1} ms: a whole number per loop", loop_ms / count)
        }
        Generator::Noise { .. } => "Repeats once per loop, without a seam".into(),
    }
}

/// The settings of one generator. Returns whether a value changed, and whether the change is finished.
fn generator_settings(ui: &mut Ui, generator: &mut Generator) -> (bool, bool) {
    match generator {
        Generator::Tone { wave, hz, level_db } => {
            let picked = wave_choice(ui, wave);
            // Each pixel dragged moves the frequency by 1 %, so low and high ones are equally easy to set.
            let speed = f64::from(hz.max(10.0) * 0.01);
            let rows = [
                ("Frequency", hz, 10.0..=20_000.0, speed, " Hz", 1),
                ("Loudest point", level_db, -60.0..=0.0, 0.1, " dB", 1),
            ];
            let (changed, done) = number_rows(ui, rows);
            (changed || picked, done || picked)
        }
        Generator::Noise {
            color,
            level_db,
            wide,
            variation,
        } => {
            let mut picked = false;
            ui.horizontal(|ui| {
                ui.label("Colour");
                for (c, hint) in [
                    (NoiseColor::White, "Even hiss"),
                    (NoiseColor::Pink, "Softer, like rain or wind"),
                    (NoiseColor::Brown, "A deep rumble"),
                ] {
                    picked |= ui.radio_value(color, c, color_name(c)).on_hover_text(hint).changed();
                }
            });
            picked |= ui
                .checkbox(wide, "Wide")
                .on_hover_text("Different noise left and right")
                .changed();
            let mut number = *variation as f32;
            let rows = [
                ("Loudest point", level_db, -60.0..=0.0, 0.1, " dB", 1),
                ("Variation", &mut number, 1.0..=9999.0, 0.2, "", 0),
            ];
            let (changed, done) = number_rows(ui, rows);
            *variation = number.round() as u32;
            (changed || picked, done || picked)
        }
        Generator::Blips {
            wave,
            hz,
            blip_ms,
            every_ms,
            level_db,
        } => {
            let picked = wave_choice(ui, wave);
            let speed = f64::from(hz.max(10.0) * 0.01);
            // Not before the last blip ended.
            let soonest = *blip_ms;
            let rows = [
                ("Frequency", hz, 10.0..=20_000.0, speed, " Hz", 1),
                ("Each blip", blip_ms, 2.0..=5000.0, 1.0, " ms", 0),
                ("One every", every_ms, soonest..=10_000.0, 1.0, " ms", 0),
                ("Loudest point", level_db, -60.0..=0.0, 0.1, " dB", 1),
            ];
            let (changed, done) = number_rows(ui, rows);
            // A longer blip pushes the next one later.
            *every_ms = every_ms.max(*blip_ms);
            (changed || picked, done || picked)
        }
    }
}

/// Returns whether another wave was picked.
fn wave_choice(ui: &mut Ui, wave: &mut Wave) -> bool {
    ui.horizontal(|ui| {
        ui.label("Wave");
        let mut picked = false;
        for w in [Wave::Sine, Wave::Triangle, Wave::Square, Wave::Saw] {
            picked |= ui.radio_value(wave, w, wave_name(w)).changed();
        }
        picked
    })
    .inner
}
