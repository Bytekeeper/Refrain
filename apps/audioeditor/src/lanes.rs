//! Effect lanes below the tracks and below the mix: their layout, drawing, and editing ranges with the mouse.

use audioeditor_engine::{Command, Effect, EffectLane, EffectRange, Frames, LaneId, Project, RangeId, TrackId};
use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Ui, pos2, vec2};

use crate::{Editor, HANDLE, HEADER_W};

pub(crate) const LANE_H: f32 = 24.0;
const EFFECT: Color32 = Color32::from_rgb(180, 130, 255);

/// A range being drawn, moved or resized. The project only changes on release, as one undo step.
pub(crate) struct RangeDrag {
    lane: LaneId,
    /// `None` while a new range is drawn.
    pub(crate) range: Option<RangeId>,
    kind: RangeDragKind,
    start: Frames,
    end: Frames,
}

enum RangeDragKind {
    New { anchor: Frames },
    Move { grab: Frames, len: Frames },
    Start,
    End,
    /// The end of the tail, `frames` after the range end.
    Tail { frames: Frames },
}

/// A range as drawn: with a drag applied, or one being drawn (`id` `None`, nothing measured yet).
struct ShownRange {
    id: Option<RangeId>,
    start: Frames,
    end: Frames,
    effect: Effect,
    gain: Option<f32>,
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

    /// The range's settings as shown: while its tail is dragged, those that give the dragged tail.
    fn shown_effect(&self, range: &EffectRange) -> Effect {
        match &self.range_drag {
            Some(RangeDrag {
                range: Some(id),
                kind: RangeDragKind::Tail { frames },
                ..
            }) if *id == range.id => range.effect.with_tail(*frames, self.session.project().sample_rate),
            _ => range.effect,
        }
    }

    /// Where the tail after a range ending at `end` is drawn: with a loop it wraps into the start, and it is
    /// at most one loop long.
    fn tail_span(&self, end: Frames, tail: Frames) -> (Frames, Frames) {
        match self.session.project().loop_end.filter(|l| *l > 0) {
            Some(l) => {
                let from = end.rem_euclid(l);
                (from, from + tail.min(l))
            }
            None => (end, end + tail),
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
                Some((_, Edge::Start | Edge::End | Edge::Tail))
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
                            Edge::Tail => RangeDragKind::Tail {
                                frames: r.effect.tail(project.sample_rate),
                            },
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
        let loop_end = project.loop_end.filter(|l| *l > 0);
        if let Some(drag) = &mut self.range_drag {
            match &mut drag.kind {
                &mut RangeDragKind::New { anchor } => {
                    let to = snap.apply(frame, &mut guide);
                    (drag.start, drag.end) = (anchor.min(to), anchor.max(to));
                }
                &mut RangeDragKind::Move { grab, len } => {
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
                RangeDragKind::Tail { frames } => {
                    let to = snap.apply(frame, &mut guide) - drag.end;
                    // With a loop the handle may sit before the range, one loop later.
                    *frames = match loop_end {
                        Some(l) => to.rem_euclid(l).max(1),
                        None => to.max(1),
                    };
                }
            }
        }
        if guide.is_some() {
            self.snap_guide = guide;
        }
        if response.drag_stopped()
            && let Some(drag) = self.range_drag.take()
        {
            let (start, end) = (drag.start, drag.end);
            let rate = project.sample_rate;
            match (drag.range, drag.kind) {
                (Some(range), RangeDragKind::Tail { frames }) => {
                    let old = project.lane(drag.lane).and_then(|l| l.ranges.iter().find(|r| r.id == range));
                    if let Some(old) = old
                        && old.effect.with_tail(frames, rate) != old.effect
                    {
                        let effect = old.effect.with_tail(frames, rate);
                        self.run(Command::SetEffect { range, effect });
                    }
                }
                (Some(range), _) => {
                    self.run(Command::SetEffectRange { range, start, end });
                }
                (None, _)
                    if end > start
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
                (None, _) => {}
            }
        }
        true
    }

    /// The range under `pos`, and which part. Range edges win over the end of another range's tail.
    fn range_at(&self, project: &Project, row: LaneRow, lane_left: f32, pos: Pos2) -> Option<(RangeId, Edge)> {
        let lane = project.lane(row.lane)?;
        let rate = project.sample_rate;
        let tail_end = |r: &EffectRange| {
            let tail = self.shown_effect(r).tail(rate);
            let (_, end) = self.shown_range(lane.id, r.id, r.start, r.end);
            let (from, to) = self.tail_span(end, tail);
            (tail > 0 && self.range_spans(lane_left, from, to).iter().any(|(_, x)| (pos.x - x).abs() <= HANDLE))
                .then_some((r.id, Edge::Tail))
        };
        let on_range = lane.ranges.iter().find_map(|r| {
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
        });
        match on_range {
            Some((id, Edge::Inside)) => lane.ranges.iter().find_map(tail_end).or(Some((id, Edge::Inside))),
            Some(hit) => Some(hit),
            None => lane.ranges.iter().find_map(tail_end),
        }
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
        let mut ranges: Vec<ShownRange> = lane
            .ranges
            .iter()
            .map(|r| {
                let (start, end) = self.shown_range(lane.id, r.id, r.start, r.end);
                ShownRange {
                    id: Some(r.id),
                    start,
                    end,
                    effect: self.shown_effect(r),
                    gain: Some(r.gain),
                }
            })
            .collect();
        if let Some(d) = &self.range_drag
            && d.lane == lane.id
            && d.range.is_none()
            && d.end > d.start
        {
            ranges.push(ShownRange {
                id: None,
                start: d.start,
                end: d.end,
                effect: lane.effect,
                gain: None,
            });
        }
        for r in &ranges {
            self.draw_tail(&painter, lane_left, body, r.end, r.effect, color);
        }
        for ShownRange {
            id,
            start,
            end,
            effect,
            gain,
        } in ranges
        {
            let selected = id.is_some() && id == self.selected_range;
            for (x0, x1) in self.range_spans(lane_left, start, end) {
                let area = Rect::from_x_y_ranges(x0..=x1, body.y_range());
                painter.rect_filled(area, 3.0, color.gamma_multiply(0.35));
                let stroke = if selected {
                    Stroke::new(2.0, Color32::WHITE)
                } else {
                    Stroke::new(1.0, color)
                };
                painter.rect_stroke(area, 3.0, stroke, egui::StrokeKind::Inside);
                let label = match (effect, gain) {
                    (Effect::Normalize { .. }, Some(gain)) => format!("{:+.1} dB", 20.0 * gain.max(1e-9).log10()),
                    (Effect::Normalize { .. }, None) => String::new(),
                    (Effect::HighPass { hz, .. }, _) => format!("above {}", hz_text(hz)),
                    (Effect::LowPass { hz, .. }, _) => format!("below {}", hz_text(hz)),
                    (Effect::Echo { delay_ms, .. }, _) => format!("every {delay_ms:.0} ms"),
                    (Effect::Reverb { seconds, .. }, _) => format!("{seconds:.1} s room"),
                };
                if x1 - x0 > 50.0 {
                    painter.text(
                        pos2(x0.max(lane_left) + 6.0, area.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                }
            }
        }
    }

    /// How long the effect keeps sounding after a range, fading out, with a handle where it is done. An echo
    /// shows a mark at each repeat. With the loop on it wraps into the start, as it is heard.
    fn draw_tail(&self, painter: &egui::Painter, lane_left: f32, body: Rect, end: Frames, effect: Effect, color: Color32) {
        let rate = self.session.project().sample_rate;
        let tail = effect.tail(rate);
        if tail <= 0 {
            return;
        }
        let (from, to) = self.tail_span(end, tail);
        let delay = match effect {
            Effect::Echo { delay_ms, .. } => (f64::from(delay_ms) * f64::from(rate) / 1000.0).round() as Frames,
            _ => 0,
        };
        for (x0, x1) in self.range_spans(lane_left, from, to) {
            let alpha = |x: f32| if x == x0 { 0.3 } else { 0.0 };
            let mut mesh = egui::Mesh::default();
            for (x, y) in [(x0, body.top()), (x1, body.top()), (x1, body.bottom()), (x0, body.bottom())] {
                mesh.colored_vertex(pos2(x, y), color.gamma_multiply(alpha(x)));
            }
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            painter.add(mesh);
            if delay > 0 {
                let per_px = (x1 - x0) / (to - from).max(1) as f32;
                let marks = (1..).map(|k| x0 + (k * delay) as f32 * per_px).take_while(|x| *x < x1 - 1.0);
                for x in marks {
                    painter.line_segment(
                        [pos2(x, body.center().y - 4.0), pos2(x, body.center().y + 4.0)],
                        Stroke::new(1.0, color.gamma_multiply(0.6)),
                    );
                }
            }
            painter.line_segment([pos2(x1, body.top()), pos2(x1, body.bottom())], Stroke::new(2.0, color));
        }
    }

    pub(crate) fn lane_menu_ui(&mut self, ui: &mut Ui, id: LaneId, range: Option<RangeId>) {
        let project = self.session.project().clone();
        let Some(lane) = project.lane(id) else {
            ui.close();
            return;
        };
        let this_range = range.and_then(|id| lane.ranges.iter().find(|r| r.id == id));
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
    Tail,
}

fn hz_text(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{} kHz", (hz / 100.0).round() / 10.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

type NumberRow<'a> = (&'static str, &'a mut f32, std::ops::RangeInclusive<f32>, f64, &'static str, usize);

/// One line per number: label and drag field. Returns whether one changed, and whether that change is finished.
fn number_rows<const N: usize>(ui: &mut Ui, rows: [NumberRow<'_>; N]) -> (bool, bool) {
    let (mut changed, mut done) = (false, false);
    for (label, value, range, speed, suffix, decimals) in rows {
        let response = ui
            .horizontal(|ui| {
                ui.label(label);
                ui.add(
                    egui::DragValue::new(value)
                        .range(range)
                        .speed(speed)
                        .fixed_decimals(decimals)
                        .suffix(suffix),
                )
            })
            .inner;
        changed |= response.changed();
        done |= response.drag_stopped() || response.lost_focus() || (response.changed() && !response.dragged());
    }
    (changed, done)
}

/// The settings of one effect. Returns whether a value changed, and whether the change is finished, which is
/// at once for a choice and on release for a dragged number.
fn effect_settings(ui: &mut Ui, effect: &mut Effect) -> (bool, bool) {
    let number = |response: egui::Response| {
        let done = response.drag_stopped() || response.lost_focus() || (response.changed() && !response.dragged());
        (response.changed(), done)
    };
    match effect {
        Effect::Normalize { peak_db } => {
            let response = ui.horizontal(|ui| {
                ui.label("Loudest point at");
                ui.add(
                    egui::DragValue::new(peak_db)
                        .range(-60.0..=0.0)
                        .speed(0.1)
                        .fixed_decimals(1)
                        .suffix(" dB"),
                )
            });
            number(response.inner)
        }
        Effect::HighPass { hz, steep } | Effect::LowPass { hz, steep } => {
            // Each pixel dragged moves the frequency by 1 %, so low and high frequencies are equally easy to set.
            let speed = hz.max(10.0) * 0.01;
            let response = ui.horizontal(|ui| {
                ui.label("Frequency");
                ui.add(
                    egui::DragValue::new(hz)
                        .range(10.0..=20_000.0)
                        .speed(speed)
                        .fixed_decimals(0)
                        .suffix(" Hz"),
                )
            });
            let (mut changed, mut done) = number(response.inner);
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
        } => {
            let rows = [
                ("Repeat every", delay_ms, 10.0..=2000.0, 1.0, " ms", 0),
                ("First echo at", level_db, -40.0..=0.0, 0.1, " dB", 1),
                ("Each next one", fade_db, 3.0..=40.0, 0.1, " dB quieter", 1),
            ];
            number_rows(ui, rows)
        }
        Effect::Reverb { seconds, level_db } => {
            let rows = [
                ("Dies away in", seconds, 0.1..=10.0, 0.01, " s", 2),
                ("Volume", level_db, -40.0..=0.0, 0.1, " dB", 1),
            ];
            number_rows(ui, rows)
        }
    }
}
