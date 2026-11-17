//! The mix level row under the ruler: the loudest point of the whole mix over time, red where it goes over
//! 0 dB. It is measured in the background after each edit, so editing never waits for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use refrain_engine::{Levels, Project};

use crate::{Editor, HEADER_W};

const ROW_H: f32 = 26.0;
const LEVEL: Color32 = Color32::from_rgb(150, 170, 190);
const OVER: Color32 = Color32::from_rgb(255, 80, 80);
/// The bars show -60 dB at the bottom up to 0 dB at a line below the top; above the line a strip marks where
/// the mix goes over, so overs show by place, not only by colour.
const FLOOR_DB: f32 = -60.0;
const OVER_H: f32 = 5.0;

#[derive(Default)]
pub(crate) struct LevelRow {
    /// The levels, and the project they were measured from.
    shown: Option<(Arc<Project>, Arc<Levels>)>,
    measuring: Option<Measuring>,
}

/// A measurement running in the background.
struct Measuring {
    project: Arc<Project>,
    stop: Arc<AtomicBool>,
    arrives: mpsc::Receiver<Option<Levels>>,
}

impl Editor {
    /// Takes a finished measurement, and starts one when the project changed since the last.
    fn measure_levels(&mut self, ctx: &egui::Context) {
        let project = self.session.project().clone();
        let row = &mut self.level_row;
        if let Some(m) = &row.measuring {
            match m.arrives.try_recv() {
                Ok(Some(levels)) => {
                    row.shown = Some((m.project.clone(), Arc::new(levels)));
                    row.measuring = None;
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => row.measuring = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let current = |p: &Arc<Project>| Arc::ptr_eq(p, &project);
        if row.shown.as_ref().is_some_and(|(p, _)| current(p))
            || row.measuring.as_ref().is_some_and(|m| current(&m.project))
        {
            return;
        }
        if let Some(m) = row.measuring.take() {
            m.stop.store(true, Ordering::Relaxed);
        }
        if project.is_empty() {
            row.shown = None;
            return;
        }
        // Tests measure at once, so every frame they look at is complete and the same each run.
        if cfg!(test) {
            let levels = refrain_engine::levels(&project, || true);
            row.shown = levels.map(|levels| (project, Arc::new(levels)));
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (send, arrives) = mpsc::channel();
        let (measured, stopped, ctx) = (project.clone(), stop.clone(), ctx.clone());
        let started = std::thread::Builder::new().name("levels".into()).spawn(move || {
            let levels = refrain_engine::levels(&measured, || !stopped.load(Ordering::Relaxed));
            let _ = send.send(levels);
            ctx.request_repaint();
        });
        if started.is_ok() {
            row.measuring = Some(Measuring { project, stop, arrives });
        }
    }

    /// The row's place under the ruler. It is painted at the end of the frame, after the tracks took the mouse,
    /// so a drop shows in the row in the same frame.
    pub(crate) fn level_row_space(&self, ui: &mut Ui) -> (Rect, egui::Response) {
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover())
    }

    pub(crate) fn draw_level_row(&mut self, ui: &Ui, (rect, response): (Rect, egui::Response)) {
        self.measure_levels(ui.ctx());
        let lane_left = rect.left() + HEADER_W;
        let painter = ui.painter_at(rect);
        painter.text(
            rect.left_center() + vec2(8.0, 0.0),
            Align2::LEFT_CENTER,
            "Mix level",
            FontId::proportional(12.0),
            ui.visuals().weak_text_color(),
        );
        let area = Rect::from_min_max(
            pos2(lane_left, rect.top() + 2.0),
            pos2(rect.right(), rect.bottom() - 2.0),
        );
        painter.rect_filled(area, 0.0, ui.visuals().extreme_bg_color);
        let zero = area.top() + OVER_H;
        let y_of = |peak: f32| {
            let db = (20.0 * peak.max(1e-9).log10()).clamp(FLOOR_DB, 0.0);
            area.bottom() - (db - FLOOR_DB) / -FLOOR_DB * (area.bottom() - zero)
        };
        if let Some((measured, levels)) = &self.level_row.shown {
            // Shown dimmed while the levels of the latest edit are still being measured.
            let fresh = Arc::ptr_eq(measured, self.session.project());
            let dim = |c: Color32| if fresh { c } else { c.gamma_multiply(0.4) };
            let end = (levels.peaks.len() * Levels::BLOCK) as i64;
            let mut x = area.left();
            while x < area.right() {
                let (from, to) = (self.frame_at(lane_left, x).max(0), self.frame_at(lane_left, x + 1.0));
                if from < end && to > 0 {
                    let peak = levels.peak(from, to.max(from + 1));
                    painter.line_segment(
                        [pos2(x, area.bottom()), pos2(x, y_of(peak))],
                        Stroke::new(1.0, dim(LEVEL)),
                    );
                    if peak > 1.0 {
                        let strip = [pos2(x, area.top()), pos2(x, zero - 1.0)];
                        painter.line_segment(strip, Stroke::new(1.0, dim(OVER)));
                    }
                }
                x += 1.0;
            }
            if let Some(pos) = response.hover_pos()
                && pos.x >= lane_left
            {
                let at = self.frame_at(lane_left, pos.x).max(0);
                let text = if at < end {
                    let peak = levels.peak(at, self.frame_at(lane_left, pos.x + 1.0).max(at + 1));
                    let level = if peak > 1e-6 {
                        format!("{:+.1} dB", 20.0 * peak.log10())
                    } else {
                        "silence".into()
                    };
                    format!("Mix {level} at {}", self.time_text(at))
                } else {
                    "Past the end of the mix".into()
                };
                response.on_hover_text_at_pointer(text);
            }
        }
        painter.line_segment(
            [pos2(area.left(), zero), pos2(area.right(), zero)],
            Stroke::new(1.0, Color32::from_gray(110)),
        );
        let play = self.x_of(lane_left, self.player.position());
        if play >= area.left() {
            painter.line_segment(
                [pos2(play, area.top()), pos2(play, area.bottom())],
                Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
            );
        }
    }
}
