//! Effect lanes below the tracks and below the mix: their layout, drawing, and editing ranges with the mouse.

use audioeditor_engine::{Command, Effect, EffectLane, Frames, LaneId, Project, RangeId, TrackId};
use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Ui, pos2, vec2};

use crate::{Editor, HANDLE, HEADER_W};

pub(crate) const LANE_H: f32 = 24.0;
const EFFECT: Color32 = Color32::from_rgb(180, 130, 255);

/// A range being drawn, moved or resized. The project only changes on release, as one undo step.
pub(crate) struct RangeDrag {
    lane: LaneId,
    /// `None` while a new range is drawn.
    range: Option<RangeId>,
    kind: RangeDragKind,
    start: Frames,
    end: Frames,
}

enum RangeDragKind {
    New { anchor: Frames },
    Move { grab: Frames, len: Frames },
    Start,
    End,
}

/// One lane on screen.
#[derive(Clone, Copy)]
struct LaneRow {
    rect: Rect,
    lane: LaneId,
}

impl Editor {
    /// The height of a track's lanes; 0 when folded away.
    pub(crate) fn lanes_height(&self, row: usize) -> f32 {
        match self.session.project().tracks.get(row) {
            Some(track) if !self.folded.contains(&track.id) => track.effects.len() as f32 * LANE_H,
            _ => 0.0,
        }
    }

    /// The Mix title row and its lanes, below everything else; nothing until the mix has an effect.
    pub(crate) fn mix_height(&self) -> f32 {
        match self.session.project().mix_effects.len() {
            0 => 0.0,
            n => (n + 1) as f32 * LANE_H,
        }
    }

    /// Right below the last track's lanes, above the row that adds tracks.
    fn mix_title(&self, rect: Rect) -> Rect {
        let tracks = self.session.project().tracks.len();
        let top = rect.top()
            + (0..tracks)
                .map(|r| self.row_height(r) + self.lanes_height(r))
                .sum::<f32>();
        Rect::from_min_size(pos2(rect.left(), top), vec2(rect.width(), LANE_H))
    }

    fn lane_rows(&self, rect: Rect) -> Vec<LaneRow> {
        let project = self.session.project();
        let mut rows = vec![];
        let row = |top: f32, k: usize| {
            Rect::from_min_size(pos2(rect.left(), top + k as f32 * LANE_H), vec2(rect.width(), LANE_H))
        };
        for (i, track) in project.tracks.iter().enumerate() {
            if self.folded.contains(&track.id) {
                continue;
            }
            let top = self.row_rect(rect, i).bottom();
            rows.extend(track.effects.iter().enumerate().map(|(k, l)| LaneRow {
                rect: row(top, k),
                lane: l.id,
            }));
        }
        let top = self.mix_title(rect).bottom();
        rows.extend(project.mix_effects.iter().enumerate().map(|(k, l)| LaneRow {
            rect: row(top, k),
            lane: l.id,
        }));
        rows
    }

    /// The range edges as shown, with the drag applied.
    fn shown_range(&self, lane: LaneId, id: RangeId, start: Frames, end: Frames) -> (Frames, Frames) {
        match &self.range_drag {
            Some(d) if d.lane == lane && d.range == Some(id) => (d.start, d.end),
            _ => (start, end),
        }
    }

    /// The screen spans of a range: with a loop, a range past the loop end also shows at the start.
    fn range_spans(&self, lane_left: f32, start: Frames, end: Frames) -> Vec<(f32, f32)> {
        let mut spans = vec![(self.x_of(lane_left, start), self.x_of(lane_left, end))];
        if let Some(l) = self.session.project().loop_end
            && end > l
        {
            spans.push((self.x_of(lane_left, start - l), self.x_of(lane_left, end - l)));
        }
        spans
    }

    /// Handles the mouse when it went down in a lane. Returns whether it did, so clips ignore that press.
    pub(crate) fn lane_mouse(&mut self, project: &Project, rect: Rect, response: &egui::Response) -> bool {
        let lane_left = rect.left() + HEADER_W;
        let rows = self.lane_rows(rect);
        let in_lane = |p: Pos2| rows.iter().find(|r| r.rect.contains(p)).copied();
        if let Some(hover) = response.hover_pos()
            && let Some(row) = in_lane(hover)
            && hover.x >= lane_left
            && matches!(
                self.range_at(project, row, lane_left, hover),
                Some((_, Edge::Start | Edge::End))
            )
        {
            response.ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        let origin = response.ctx.input(|i| i.pointer.press_origin());
        let Some(pos) = response.interact_pointer_pos() else {
            return false;
        };
        let frame = self.frame_at(lane_left, pos.x);
        if response.secondary_clicked()
            && let Some(row) = in_lane(pos)
        {
            let range = self.range_at(project, row, lane_left, pos).map(|(id, _)| id);
            self.menu = Some(crate::mouse::Menu::Lane(row.lane, range));
            return true;
        }
        if response.clicked()
            && let Some(row) = in_lane(pos)
        {
            self.selected_range = self.range_at(project, row, lane_left, pos).map(|(id, _)| id);
            if self.selected_range.is_some() {
                self.selected = None;
            }
            return true;
        }
        // A click knows only where it ended; a drag is judged by where it began.
        let pressed_in = origin.and_then(in_lane);
        if self.range_drag.is_none() && pressed_in.is_none() {
            return false;
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(row) = pressed_in
            && let Some(origin) = origin
            && origin.x >= lane_left
        {
            let at = self.frame_at(lane_left, origin.x);
            let lane = project.lane(row.lane);
            self.range_drag = match self.range_at(project, row, lane_left, origin) {
                Some((id, edge)) => {
                    let range = lane.and_then(|l| l.ranges.iter().find(|r| r.id == id));
                    range.map(|r| RangeDrag {
                        lane: row.lane,
                        range: Some(id),
                        kind: match edge {
                            Edge::Start => RangeDragKind::Start,
                            Edge::End => RangeDragKind::End,
                            Edge::Inside => RangeDragKind::Move {
                                grab: at - r.start,
                                len: r.end - r.start,
                            },
                        },
                        start: r.start,
                        end: r.end,
                    })
                }
                None => Some(RangeDrag {
                    lane: row.lane,
                    range: None,
                    kind: RangeDragKind::New { anchor: at },
                    start: at,
                    end: at,
                }),
            };
            self.selected_range = self.range_drag.as_ref().and_then(|d| d.range);
            self.selected = None;
        }
        let shift = response.ctx.input(|i| i.modifiers.shift);
        let snap = self.snap_targets(project, None, shift);
        let mut guide = None;
        if let Some(drag) = &mut self.range_drag {
            match drag.kind {
                RangeDragKind::New { anchor } => {
                    let to = snap.apply(frame, &mut guide);
                    (drag.start, drag.end) = (anchor.min(to), anchor.max(to));
                }
                RangeDragKind::Move { grab, len } => {
                    let start = frame - grab;
                    let snapped = snap.apply(start, &mut guide);
                    let start = if snapped != start {
                        snapped
                    } else {
                        snap.apply(start + len, &mut guide) - len
                    };
                    (drag.start, drag.end) = (start, start + len);
                }
                RangeDragKind::Start => drag.start = snap.apply(frame, &mut guide).min(drag.end - 1),
                RangeDragKind::End => drag.end = snap.apply(frame, &mut guide).max(drag.start + 1),
            }
        }
        if guide.is_some() {
            self.snap_guide = guide;
        }
        if response.drag_stopped()
            && let Some(drag) = self.range_drag.take()
        {
            let (start, end) = (drag.start, drag.end);
            match drag.range {
                Some(range) => {
                    self.run(Command::SetEffectRange { range, start, end });
                }
                None if end > start
                    && self.run(Command::AddEffectRange {
                        lane: drag.lane,
                        start,
                        end,
                    }) =>
                {
                    let ranges = self.session.project().lane(drag.lane).map(|l| l.ranges.clone());
                    self.selected_range = ranges
                        .and_then(|r| r.into_iter().find(|r| r.start == start))
                        .map(|r| r.id);
                }
                None => {}
            }
        }
        true
    }

    fn range_at(&self, project: &Project, row: LaneRow, lane_left: f32, pos: Pos2) -> Option<(RangeId, Edge)> {
        let lane = project.lane(row.lane)?;
        lane.ranges.iter().find_map(|r| {
            let (start, end) = self.shown_range(lane.id, r.id, r.start, r.end);
            self.range_spans(lane_left, start, end)
                .into_iter()
                .find_map(|(x0, x1)| {
                    if (pos.x - x0).abs() <= HANDLE {
                        Some((r.id, Edge::Start))
                    } else if (pos.x - x1).abs() <= HANDLE {
                        Some((r.id, Edge::End))
                    } else {
                        (pos.x > x0 && pos.x < x1).then_some((r.id, Edge::Inside))
                    }
                })
        })
    }

    pub(crate) fn draw_lanes(&mut self, ui: &mut Ui, rect: Rect, project: &Project) {
        let lane_left = rect.left() + HEADER_W;
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals().clone();
        if !project.mix_effects.is_empty() {
            let title = self.mix_title(rect);
            painter.rect_filled(title, 0.0, visuals.faint_bg_color);
            painter.text(
                title.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                "Mix",
                FontId::proportional(13.0),
                visuals.text_color(),
            );
            painter.text(
                pos2(lane_left + 8.0, title.center().y),
                Align2::LEFT_CENTER,
                "effects on all tracks together",
                FontId::proportional(11.0),
                visuals.weak_text_color(),
            );
        }
        for row in self.lane_rows(rect) {
            let Some(lane) = project.lane(row.lane) else { continue };
            let header = Rect::from_min_max(row.rect.min, pos2(lane_left, row.rect.bottom()));
            painter.rect_filled(header, 0.0, visuals.extreme_bg_color);
            painter.rect_filled(
                row.rect.with_min_x(lane_left),
                0.0,
                visuals.extreme_bg_color.gamma_multiply(0.8),
            );
            painter.line_segment(
                [row.rect.left_top(), row.rect.right_top()],
                Stroke::new(0.5, visuals.weak_text_color().gamma_multiply(0.5)),
            );
            self.draw_ranges(&painter, lane, row, lane_left);
            let mut enabled = lane.enabled;
            let response = ui
                .put(
                    Rect::from_min_size(header.min + vec2(4.0, 2.0), vec2(HEADER_W - 8.0, LANE_H - 4.0)),
                    egui::Checkbox::new(&mut enabled, lane.effect.name()),
                )
                .on_hover_text("On or off, to compare. Right-click: settings, order, remove");
            if response.changed() {
                self.run(Command::SetEffectEnabled { lane: lane.id, enabled });
            }
            response.context_menu(|ui| self.lane_menu_ui(ui, lane.id, None));
        }
    }

    fn draw_ranges(&self, painter: &egui::Painter, lane: &EffectLane, row: LaneRow, lane_left: f32) {
        let body = row.rect.with_min_x(lane_left).shrink2(vec2(0.0, 3.0));
        let painter = painter.with_clip_rect(row.rect.with_min_x(lane_left));
        let color = if lane.enabled { EFFECT } else { Color32::GRAY };
        let rate = self.session.project().sample_rate;
        let mut ranges: Vec<(Option<RangeId>, Frames, Frames, Option<f32>)> = lane
            .ranges
            .iter()
            .map(|r| {
                let (start, end) = self.shown_range(lane.id, r.id, r.start, r.end);
                (Some(r.id), start, end, Some(r.gain))
            })
            .collect();
        if let Some(d) = &self.range_drag
            && d.lane == lane.id
            && d.range.is_none()
            && d.end > d.start
        {
            ranges.push((None, d.start, d.end, None));
        }
        for (id, start, end, gain) in ranges {
            let selected = id.is_some() && id == self.selected_range;
            for (x0, x1) in self.range_spans(lane_left, start, end) {
                let area = Rect::from_x_y_ranges(x0..=x1, body.y_range());
                let lead = self.px_of(lane.effect.lead_in(rate));
                let tail = self.px_of(lane.effect.tail(rate));
                if lead > 0.0 {
                    painter.rect_filled(
                        Rect::from_x_y_ranges((x0 - lead)..=x0, body.y_range()),
                        0.0,
                        color.gamma_multiply(0.12),
                    );
                }
                if tail > 0.0 {
                    painter.rect_filled(
                        Rect::from_x_y_ranges(x1..=(x1 + tail), body.y_range()),
                        0.0,
                        color.gamma_multiply(0.12),
                    );
                }
                painter.rect_filled(area, 3.0, color.gamma_multiply(0.35));
                let stroke = if selected {
                    Stroke::new(2.0, Color32::WHITE)
                } else {
                    Stroke::new(1.0, color)
                };
                painter.rect_stroke(area, 3.0, stroke, egui::StrokeKind::Inside);
                if let (Some(gain), Effect::Normalize { .. }) = (gain, lane.effect)
                    && x1 - x0 > 40.0
                {
                    let db = 20.0 * gain.max(1e-9).log10();
                    painter.text(
                        pos2(x0.max(lane_left) + 6.0, area.center().y),
                        Align2::LEFT_CENTER,
                        format!("{db:+.1} dB"),
                        FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                }
            }
        }
    }

    fn px_of(&self, frames: Frames) -> f32 {
        (frames as f64 / self.frames_per_px.max(1e-9)) as f32
    }

    pub(crate) fn lane_menu_ui(&mut self, ui: &mut Ui, id: LaneId, range: Option<RangeId>) {
        let project = self.session.project().clone();
        let Some(lane) = project.lane(id) else {
            ui.close();
            return;
        };
        ui.label(lane.effect.name());
        match lane.effect {
            Effect::Normalize { peak_db } => {
                let mut db = match self.effect_edit {
                    Some((l, Effect::Normalize { peak_db })) if l == id => peak_db,
                    _ => peak_db,
                };
                let response = ui
                    .horizontal(|ui| {
                        ui.label("Loudest point at");
                        ui.add(
                            egui::DragValue::new(&mut db)
                                .range(-60.0..=0.0)
                                .speed(0.1)
                                .fixed_decimals(1)
                                .suffix(" dB"),
                        )
                    })
                    .inner;
                if response.changed() {
                    self.effect_edit = Some((id, Effect::Normalize { peak_db: db }));
                }
                let done =
                    response.drag_stopped() || response.lost_focus() || (response.changed() && !response.dragged());
                if done && let Some((lane, effect)) = self.effect_edit.take() {
                    self.run(Command::SetEffect { lane, effect });
                }
            }
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

enum Edge {
    Start,
    End,
    Inside,
}
