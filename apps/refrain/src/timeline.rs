//! The timeline: track rows with their headers, the ruler, the scrollbar and the layout they share.

use egui::{Align2, Color32, FontId, Key, Rect, Sense, Stroke, Ui, pos2, vec2};
use refrain_engine::{Clip, Fade, Frames, MAX_DB, MIN_DB, Project, TrackId, edge_fades, overlap_after, overlap_before};

use crate::clips::{Look, draw_trash_can, trash_button};
use crate::mouse::Drag;
use crate::{Command, Editor, HANDLE, HEADER_W, TRACK_H};

const RULER_H: f32 = 22.0;
pub(crate) const SCROLLBAR_H: f32 = 12.0;
/// Room for the name, the mute and volume controls and the delete button.
pub(crate) const MIN_TRACK_H: f32 = 80.0;
pub(crate) const MAX_TRACK_H: f32 = 800.0;
pub(crate) const CURVE: Color32 = Color32::from_rgb(255, 200, 80);
pub(crate) const LOOP: Color32 = Color32::from_rgb(90, 200, 120);
/// The guide line while a dragged edge sits on a snap target.
const SNAP: Color32 = Color32::from_rgb(120, 220, 255);

impl Editor {
    pub(crate) fn timeline(&mut self, ui: &mut Ui) {
        let project = self.session.project().clone();
        // One row more than there are tracks: it holds the Add track button and takes dropped clips.
        let rows = 0..=project.tracks.len();
        let height: f32 = rows
            .map(|row| self.row_height(row) + self.lanes_height(row))
            .sum::<f32>()
            + self.mix_height();
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
        let lane_left = rect.left() + HEADER_W;
        self.lane_w = rect.width() - HEADER_W;

        if let Some(pos) = response.hover_pos() {
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.x));
            if zoom != 1.0 {
                let anchor = self.frame_at(lane_left, pos.x);
                self.frames_per_px = (self.frames_per_px / f64::from(zoom)).clamp(1.0, 200_000.0);
                self.scroll = anchor as f64 - f64::from(pos.x - lane_left) * self.frames_per_px;
            }
            self.scroll -= f64::from(scroll) * self.frames_per_px;
            self.clamp_scroll(rect.width() - HEADER_W);
            if ui.input(|i| i.key_pressed(Key::S))
                && let Some(clip) = self.selected
            {
                self.run(Command::SplitClip {
                    clip,
                    at: self.frame_at(lane_left, pos.x),
                });
            }
        }
        let selected = self.selected;
        if !self.resize_track(&project, rect, &response) && !self.lane_mouse(&project, rect, &response) {
            self.handle_mouse(&project, rect, &response);
        }
        if self.selected.is_some() && self.selected != selected {
            self.selected_range = None;
        }
        if let Some(menu) = self.menu
            && response.context_menu(|ui| self.menu_ui(ui, menu)).is_none()
        {
            self.menu = None;
        }
        // A drop or menu command above changed the project; drawing the copy from before shows the old state for a frame.
        let project = self.session.project().clone();

        let painter = ui.painter_at(rect);
        let text = ui.visuals().text_color();
        for (i, track) in project.tracks.iter().enumerate() {
            let row = self.row_rect(rect, i);
            painter.rect_filled(
                Rect::from_min_max(row.min, pos2(lane_left, row.bottom())),
                0.0,
                ui.visuals().faint_bg_color,
            );
            let bottom = row.bottom() + self.lanes_height(i);
            painter.line_segment(
                [pos2(row.left(), bottom), pos2(row.right(), bottom)],
                Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
            self.track_controls(ui, row, track);
        }
        self.draw_lanes(ui, rect, &project);
        let lanes = ui.painter_at(Rect::from_min_max(pos2(lane_left, rect.top()), rect.max));
        let new_row = project.tracks.len();
        let last_row = self.row_rect(rect, new_row);
        let first_row = self.row_rect(rect, 0);
        let dropping_new = matches!(self.drag, Some(Drag::Move { track: None, .. }));
        let add = Rect::from_min_size(last_row.min + vec2(6.0, 8.0), vec2(80.0, 20.0));
        if !dropping_new && ui.put(add, egui::Button::new("Add track").small()).clicked() {
            let name = format!("Track {}", project.tracks.len() + 1);
            self.run(Command::AddTrack { name });
        }
        if !dropping_new {
            let mix = Rect::from_min_size(add.right_top() + vec2(12.0, 0.0), vec2(60.0, 20.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(mix), |ui| {
                ui.menu_button("Mix FX", |ui| self.effects_menu_ui(ui, None))
                    .response
                    .on_hover_text("Effects on all tracks together, such as normalizing the whole loop");
            });
        }
        if dropping_new {
            let row = last_row.shrink(2.0);
            let dash = Stroke::new(1.0, ui.visuals().weak_text_color());
            let corners = [
                row.left_top(),
                row.right_top(),
                row.right_bottom(),
                row.left_bottom(),
                row.left_top(),
            ];
            painter.extend(egui::Shape::dashed_line(&corners, dash, 6.0, 4.0));
            painter.text(
                row.min + vec2(8.0, 8.0),
                Align2::LEFT_TOP,
                "New track",
                FontId::proportional(13.0),
                text,
            );
        } else if project.tracks.is_empty() {
            painter.text(
                first_row.center(),
                Align2::CENTER_CENTER,
                "Import audio or add a track",
                FontId::proportional(14.0),
                text,
            );
        }
        let mut ids: Vec<Option<TrackId>> = project.tracks.iter().map(|t| Some(t.id)).collect();
        ids.push(None);
        let tracks: Vec<Vec<Clip>> = ids.into_iter().map(|t| self.preview_track(&project, t)).collect();
        let wrap = self.loop_end(&project);
        let wrapped: Vec<Vec<(Clip, (Fade, Fade))>> = tracks.iter().map(|clips| wrapped_parts(clips, wrap)).collect();
        let mut on_top = None;
        for (row, clips) in tracks.iter().enumerate() {
            for index in self.stacking(clips).into_iter().rev() {
                let Some(clip) = clips.get(index) else { continue };
                let fades = edge_fades(clips, index, wrap);
                let free = free_ends(clips, index, wrap);
                let area = self.clip_area(rect, row, clip.start, clip.len);
                if area.right() <= lane_left {
                    continue;
                }
                let dragged = self.drag.as_ref().is_some_and(|d| d.clip() == clip.id);
                if dragged || Some(clip.id) == self.selected {
                    on_top = Some((area, clip.clone(), fades, free));
                    continue;
                }
                self.draw_clip(&lanes, area, lane_left, clip, (fades, free), Look::Normal);
            }
        }
        for (row, parts) in wrapped.iter().enumerate() {
            for (part, fades) in parts {
                let area = self.clip_area(rect, row, part.start, part.len);
                self.draw_clip(&lanes, area, lane_left, part, (*fades, (false, false)), Look::Wrapped);
            }
        }
        if let Some(Drag::TrimStart { clip, .. } | Drag::TrimEnd { clip, .. }) = &self.drag
            && let Some((track, c)) = project.clip(*clip)
            && let Some(row) = project.tracks.iter().position(|t| t.id == track.id)
        {
            // How far the edges can go: the whole audio file.
            let whole = self.clip_area(rect, row, c.start - c.offset, c.source.frames());
            let r = whole.shrink(1.0).intersect(lanes.clip_rect().expand(2.0));
            let corners = [
                r.left_top(),
                r.right_top(),
                r.right_bottom(),
                r.left_bottom(),
                r.left_top(),
            ];
            let dash = Stroke::new(1.0, Color32::from_gray(170));
            lanes.extend(egui::Shape::dashed_line(&corners, dash, 6.0, 4.0));
        }
        let selected_area = on_top
            .as_ref()
            .filter(|(_, clip, ..)| Some(clip.id) == self.selected)
            .map(|(area, ..)| *area);
        if let Some((area, clip, fades, free)) = on_top {
            self.draw_clip(&lanes, area, lane_left, &clip, (fades, free), Look::Selected);
        }
        if let Some(l) = wrap {
            // Past the loop end nothing plays there: that audio is heard at the start.
            let x = self.x_of(lane_left, l);
            let past = Rect::from_x_y_ranges(x.max(lane_left)..=rect.right().max(x), rect.y_range());
            lanes.rect_filled(past, 0.0, Color32::from_black_alpha(110));
            lanes.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, LOOP));
        }
        for (row, clips) in tracks.iter().enumerate() {
            let parts = wrapped.get(row).map_or(&[][..], Vec::as_slice);
            self.draw_overlaps(&lanes, rect, row, clips, wrap, parts);
        }
        for (row, _) in project.tracks.iter().enumerate().filter(|(_, t)| t.muted) {
            let lane = self.row_rect(rect, row).with_min_x(lane_left);
            lanes.rect_filled(lane, 0.0, Color32::from_black_alpha(130));
        }
        if let Some(target) = self.snap_guide {
            let x = self.x_of(lane_left, target);
            lanes.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, SNAP));
        }
        // Last, so no crossfade view covers it.
        if let Some(button) = selected_area.and_then(|area| trash_button(area, lanes.clip_rect().right())) {
            lanes.rect_filled(button, 3.0, Color32::from_black_alpha(160));
            draw_trash_can(&lanes, button.shrink(5.0), Stroke::new(1.5, Color32::WHITE));
        }
        let x = self.x_of(lane_left, self.player.position());
        if x >= lane_left {
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
            );
        }
    }

    /// Mute, volume and delete in the track's header.
    /// The track's name, shortened with "…" when it does not fit; double-click to rename.
    fn track_name(&mut self, ui: &mut Ui, row: Rect, track: &refrain_engine::Track) {
        let place = Rect::from_min_size(row.min + vec2(6.0, 4.0), vec2(HEADER_W - 12.0, 22.0));
        let left = egui::Layout::left_to_right(egui::Align::Center);
        ui.scope_builder(egui::UiBuilder::new().max_rect(place).layout(left), |ui| {
            let editing = matches!(&self.name_edit, Some((t, _)) if *t == track.id);
            if !editing {
                let label = egui::Label::new(egui::RichText::new(&track.name).size(13.0))
                    .truncate()
                    .sense(Sense::click());
                if ui
                    .add(label)
                    .on_hover_text(format!("{}\nDouble-click to rename", track.name))
                    .double_clicked()
                {
                    self.name_edit = Some((track.id, track.name.clone()));
                }
                return;
            }
            let Some((_, text)) = &mut self.name_edit else { return };
            let response = ui
                .add(egui::TextEdit::singleline(text).desired_width(place.width()))
                .accessible_name("Track name");
            if !response.has_focus() && !response.lost_focus() {
                response.request_focus();
            }
            if response.lost_focus() {
                let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if let Some((id, name)) = self.name_edit.take()
                    && !cancelled
                    && !name.trim().is_empty()
                    && name.trim() != track.name
                {
                    self.run(Command::RenameTrack { track: id, name });
                }
            }
        });
    }

    fn track_controls(&mut self, ui: &mut Ui, row: Rect, track: &refrain_engine::Track) {
        self.track_name(ui, row, track);
        let mute = Rect::from_min_size(row.min + vec2(6.0, 30.0), vec2(24.0, 20.0));
        if ui
            .put(mute, egui::Button::selectable(track.muted, "M"))
            .on_hover_text(if track.muted {
                "Muted: click to hear this track"
            } else {
                "Mute this track"
            })
            .clicked()
        {
            self.run(Command::SetTrackMute {
                track: track.id,
                muted: !track.muted,
            });
        }

        // While dragging, the value lives here; the project only changes on release, as one undo step.
        let mut db = match self.gain_edit {
            Some((t, db)) if t == track.id => db,
            _ => track.gain_db,
        };
        let gain = Rect::from_min_size(row.min + vec2(34.0, 30.0), vec2(68.0, 20.0));
        let response = ui
            .put(
                gain,
                egui::DragValue::new(&mut db)
                    .range(-60.0..=12.0)
                    .speed(0.1)
                    .fixed_decimals(1)
                    .suffix(" dB"),
            )
            .on_hover_text("Track volume: drag sideways or click to type");
        if response.changed() {
            self.gain_edit = Some((track.id, db));
        }
        let done = response.drag_stopped() || (response.changed() && !response.dragged());
        if done && let Some((track, db)) = self.gain_edit.take() {
            self.run(Command::SetTrackGain { track, db });
        }

        let delete = Rect::from_min_size(pos2(row.left() + 6.0, row.bottom() - 28.0), vec2(24.0, 24.0));
        let fx = Rect::from_min_size(delete.right_top() + vec2(4.0, 2.0), vec2(56.0, 20.0));
        let label = match track.effects.len() {
            0 => "FX".to_string(),
            n => format!("FX {n}"),
        };
        ui.scope_builder(egui::UiBuilder::new().max_rect(fx), |ui| {
            ui.menu_button(label, |ui| self.effects_menu_ui(ui, Some(track.id)))
                .response
                .on_hover_text("Effects on this track: add one, or hide and show its lanes");
        });
        let response = ui.put(delete, egui::Button::new("")).on_hover_text("Delete this track");
        draw_trash_can(
            ui.painter(),
            delete.shrink(6.0),
            Stroke::new(1.5, ui.visuals().text_color()),
        );
        if response.clicked() {
            self.run(Command::RemoveTrack { track: track.id });
        }
    }

    /// A track's clips as they look with the current drag applied, sorted by start.
    /// `None` is the row below all tracks, which only holds a clip while it is dragged there.
    fn preview_track(&self, project: &Project, track: Option<TrackId>) -> Vec<Clip> {
        let moving = match &self.drag {
            Some(Drag::Move { clip, track: to, .. }) => Some((*clip, *to)),
            _ => None,
        };
        let mut clips: Vec<Clip> = track
            .and_then(|t| project.track(t))
            .map(|t| {
                t.clips
                    .iter()
                    .filter(|c| moving.is_none_or(|(id, to)| c.id != id || to == track))
                    .map(|c| self.preview(c))
                    .collect()
            })
            .unwrap_or_default();
        if let Some((id, to)) = moving
            && to == track
            && !clips.iter().any(|c| c.id == id)
            && let Some((_, c)) = project.clip(id)
        {
            clips.push(self.preview(c));
        }
        clips.sort_by_key(|c| c.start);
        clips
    }

    /// The clip as it looks with the current drag applied.
    fn preview(&self, clip: &Clip) -> Clip {
        let mut c = clip.clone();
        match &self.drag {
            Some(Drag::Move { clip, start, .. }) if *clip == c.id => c.start = *start,
            Some(Drag::TrimStart { clip, start }) if *clip == c.id => {
                let end = c.end();
                let _ = c.trim(*start, end);
            }
            Some(Drag::TrimEnd { clip, end }) if *clip == c.id => {
                let start = c.start;
                let _ = c.trim(start, *end);
            }
            Some(Drag::FadeIn { clip, len }) if *clip == c.id => c.fade_in.len = *len,
            Some(Drag::FadeOut { clip, len }) if *clip == c.id => c.fade_out.len = *len,
            Some(Drag::Point { clip, index, point }) if *clip == c.id => {
                if let Some(p) = c.gain.get_mut(*index) {
                    *p = *point;
                }
                c.gain.sort_by_key(|p| p.at);
            }
            _ => {}
        }
        c
    }

    /// Time labels above the tracks; clicking moves the play position, dragging the flag moves the loop end.
    pub(crate) fn ruler(&mut self, ui: &mut Ui) {
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), RULER_H), Sense::click_and_drag());
        let lane_left = rect.left() + HEADER_W;
        let project = self.session.project().clone();
        let loop_x = self.loop_end(&project).map(|l| self.x_of(lane_left, l));
        let origin = ui.input(|i| i.pointer.press_origin());
        let on_flag = loop_x.zip(origin).is_some_and(|(x, o)| (o.x - x).abs() <= HANDLE + 2.0);
        if loop_x
            .zip(response.hover_pos())
            .is_some_and(|(x, p)| (p.x - x).abs() <= HANDLE + 2.0)
            || self.loop_drag.is_some()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if response.drag_started() && on_flag {
            self.loop_drag = project.loop_end;
        }
        if let Some(pos) = response.interact_pointer_pos() {
            let frame = self.frame_at(lane_left, pos.x);
            if self.loop_drag.is_some() {
                let shift = ui.input(|i| i.modifiers.shift);
                let snapped = self
                    .snap_targets(&project, None, shift)
                    .apply(frame, &mut self.snap_guide);
                self.loop_drag = Some(snapped.max(1));
            } else if !on_flag {
                // Left of the time labels is the left edge of what is shown.
                self.player.seek(self.frame_at(lane_left, pos.x.max(lane_left)));
            }
        }
        if response.drag_stopped()
            && let Some(at) = self.loop_drag.take()
        {
            self.run(Command::SetLoopEnd { at: Some(at) });
        }
        let project = self.session.project().clone();
        let rate = self.session.project().sample_rate.max(1);
        let painter = ui.painter_at(Rect::from_min_max(pos2(lane_left, rect.top()), rect.max));
        let color = ui.visuals().weak_text_color();
        let secs_per_px = self.frames_per_px / f64::from(rate);
        let step = [
            0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
        ]
        .into_iter()
        .find(|s| s / secs_per_px >= 90.0)
        .unwrap_or(1800.0);
        let first = (self.scroll / f64::from(rate) / step).floor() as i64;
        let count = ((rect.width() as f64 * secs_per_px) / step).ceil() as i64 + 2;
        for n in first..first.saturating_add(count) {
            let secs = n as f64 * step;
            let x = self.x_of(lane_left, (secs * f64::from(rate)).round() as Frames);
            painter.line_segment(
                [pos2(x, rect.bottom() - 6.0), pos2(x, rect.bottom())],
                Stroke::new(1.0, color),
            );
            let label = format!(
                "{}:{:0w$.p$}",
                (secs / 60.0).floor(),
                secs % 60.0,
                w = if step < 1.0 { 5 } else { 2 },
                p = if step < 0.1 {
                    2
                } else if step < 1.0 {
                    1
                } else {
                    0
                }
            );
            painter.text(
                pos2(x + 3.0, rect.top() + 3.0),
                Align2::LEFT_TOP,
                label,
                FontId::proportional(11.0),
                color,
            );
        }
        if let Some(x) = self.loop_end(&project).map(|l| self.x_of(lane_left, l)) {
            let flag = vec![pos2(x, rect.top()), pos2(x, rect.bottom()), pos2(x - 12.0, rect.top())];
            painter.add(egui::Shape::convex_polygon(flag, LOOP, Stroke::NONE));
        }
        let x = self.x_of(lane_left, self.player.position());
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
        );
    }

    /// The loop end as shown: while its flag is dragged, where the flag is.
    pub(crate) fn loop_end(&self, project: &Project) -> Option<Frames> {
        self.loop_drag.or(project.loop_end)
    }

    /// Shows which part of the project is visible; drag the handle or click the bar to move.
    pub(crate) fn scrollbar(&mut self, ui: &mut Ui) {
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), SCROLLBAR_H), Sense::click_and_drag());
        let bar = Rect::from_min_max(pos2(rect.left() + HEADER_W, rect.top()), rect.max);
        let visible = f64::from(bar.width()) * self.frames_per_px;
        let total = self.scroll_total(bar.width());
        let px_per_frame = f64::from(bar.width()) / total;
        let thumb_w = ((visible * px_per_frame) as f32).max(20.0).min(bar.width());
        let thumb_x = bar.left() + (self.scroll * px_per_frame) as f32;
        let thumb = Rect::from_min_size(pos2(thumb_x, bar.top() + 2.0), vec2(thumb_w, bar.height() - 4.0));
        if response.dragged() {
            self.scroll += f64::from(response.drag_delta().x) / px_per_frame;
        } else if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.scroll = f64::from(pos.x - bar.left()) / px_per_frame - visible / 2.0;
        }
        self.clamp_scroll(bar.width());
        let painter = ui.painter();
        painter.rect_filled(bar, 3.0, ui.visuals().extreme_bg_color);
        let fill = if response.hovered() || response.dragged() {
            Color32::from_gray(170)
        } else {
            Color32::from_gray(120)
        };
        painter.rect_filled(thumb, 3.0, fill);
    }

    /// The scrollable length: the project plus half a screen, so the end is never stuck at the edge.
    fn scroll_total(&self, lane_w: f32) -> f64 {
        let visible = f64::from(lane_w.max(1.0)) * self.frames_per_px;
        let project = self.session.project();
        let end = project.len().max(project.clips_end());
        (end as f64 + visible / 2.0).max(visible)
    }

    pub(crate) fn clamp_scroll(&mut self, lane_w: f32) {
        let visible = f64::from(lane_w.max(1.0)) * self.frames_per_px;
        let max = (self.scroll_total(lane_w) - visible).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    pub(crate) fn clip_area(&self, rect: Rect, row: usize, start: Frames, len: Frames) -> Rect {
        clip_area(
            self.row_rect(rect, row),
            rect.left() + HEADER_W,
            (start, len),
            self.scroll,
            self.frames_per_px,
        )
    }

    /// Each track keeps the height it was dragged to; the row below all tracks has the default.
    pub(crate) fn row_height(&self, row: usize) -> f32 {
        let track = self.session.project().tracks.get(row).map(|t| t.id);
        track
            .and_then(|t| self.track_heights.get(&t))
            .copied()
            .unwrap_or(TRACK_H)
    }

    pub(crate) fn row_rect(&self, rect: Rect, row: usize) -> Rect {
        let mut top = rect.top() + (0..row).map(|r| self.row_height(r) + self.lanes_height(r)).sum::<f32>();
        if row >= self.session.project().tracks.len() {
            top += self.mix_height();
        }
        Rect::from_min_size(pos2(rect.left(), top), vec2(rect.width(), self.row_height(row)))
    }

    /// The row at height `y`: above the first row counts as the first, below the last as the row after all tracks.
    pub(crate) fn row_at(&self, rect: Rect, y: f32) -> usize {
        let rows = self.session.project().tracks.len() + 1;
        (0..rows)
            .find(|&r| y < self.row_rect(rect, r).bottom() + self.lanes_height(r))
            .unwrap_or(rows - 1)
    }

    /// Dragging the line below a track changes its height. Returns whether the mouse is busy with that.
    fn resize_track(&mut self, project: &Project, rect: Rect, response: &egui::Response) -> bool {
        let near_line =
            |e: &Editor, y: f32| (0..project.tracks.len()).find(|&r| (e.row_rect(rect, r).bottom() - y).abs() <= 3.0);
        if response.hover_pos().is_some_and(|p| near_line(self, p.y).is_some()) || self.resizing.is_some() {
            response.ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(origin) = response.ctx.input(|i| i.pointer.press_origin())
            && let Some(row) = near_line(self, origin.y)
        {
            self.resizing = project.tracks.get(row).map(|t| t.id);
        }
        let Some(track) = self.resizing else { return false };
        if let Some(pos) = response.interact_pointer_pos()
            && let Some(row) = project.tracks.iter().position(|t| t.id == track)
        {
            let top = self.row_rect(rect, row).top();
            self.track_heights
                .insert(track, (pos.y - top).clamp(MIN_TRACK_H, MAX_TRACK_H));
        }
        if response.drag_stopped() {
            self.resizing = None;
        }
        true
    }

    pub(crate) fn x_of(&self, lane_left: f32, frame: Frames) -> f32 {
        lane_left + ((frame as f64 - self.scroll) / self.frames_per_px) as f32
    }

    pub(crate) fn frame_at(&self, lane_left: f32, x: f32) -> Frames {
        (self.scroll + f64::from(x - lane_left) * self.frames_per_px).round() as Frames
    }
}

/// Where a clip of `(start, len)` frames is drawn in the track row `row`.
pub(crate) fn clip_area(
    row: Rect,
    lane_left: f32,
    (start, len): (Frames, Frames),
    scroll: f64,
    frames_per_px: f64,
) -> Rect {
    let x = |f: Frames| lane_left + ((f as f64 - scroll) / frames_per_px) as f32;
    Rect::from_x_y_ranges(
        x(start)..=x(start.saturating_add(len)),
        row.top() + 2.0..=row.bottom() - 2.0,
    )
}

/// 0 dB sits in the middle of the clip, +6 dB at the top.
pub(crate) fn y_of_amp(area: Rect, amp: f32) -> f32 {
    area.bottom() - 2.0 - amp.clamp(0.0, 2.0) / 2.0 * (area.height() - 4.0)
}

pub(crate) fn db_of_y(area: Rect, y: f32) -> f32 {
    let amp = (area.bottom() - 2.0 - y) / (area.height() - 4.0).max(1.0) * 2.0;
    if amp <= 0.0 {
        MIN_DB
    } else {
        (20.0 * amp.log10()).clamp(MIN_DB, MAX_DB.min(6.0))
    }
}

pub(crate) fn db_to_amp(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// Whether the start and the end of clip `i` are free of neighbours (and so may have their own fade).
pub(crate) fn free_ends(clips: &[Clip], i: usize, loop_end: Option<Frames>) -> (bool, bool) {
    (
        overlap_before(clips, i, loop_end) == 0,
        overlap_after(clips, i, loop_end) == 0,
    )
}

/// The parts of clips past the loop end, moved to where they play: the start of the loop.
/// Each keeps the fade-out of its clip, which is the crossfade into the start.
fn wrapped_parts(clips: &[Clip], wrap: Option<Frames>) -> Vec<(Clip, (Fade, Fade))> {
    let Some(l) = wrap else { return vec![] };
    let mut parts = vec![];
    for (i, clip) in clips.iter().enumerate().filter(|(_, c)| c.end() > l) {
        let mut part = clip.clone();
        if part.trim(l.max(clip.start), clip.end()).is_ok() {
            part.start -= l;
            parts.push((part, (Fade::default(), edge_fades(clips, i, wrap).1)));
        }
    }
    parts
}
