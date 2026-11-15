//! Drawing the timeline: tracks, clips, crossfades, the ruler and the scrollbar.

use std::sync::Arc;

use audioeditor_engine::{Clip, Fade, Frames, MAX_DB, MIN_DB, Project, TrackId, clip_amp, edge_fades};
use craft_audio::peaks::Peaks;
use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::mouse::Drag;
use crate::{Command, Editor, HANDLE, HEADER_W, TRACK_H};

const RULER_H: f32 = 22.0;
pub(crate) const SCROLLBAR_H: f32 = 12.0;
/// Translucent orange over the frames where two clips crossfade.
const OVERLAP_FILL: Color32 = Color32::from_rgba_unmultiplied_const(255, 140, 0, 120);
pub(crate) const CURVE: Color32 = Color32::from_rgb(255, 200, 80);

impl Editor {
    pub(crate) fn timeline(&mut self, ui: &mut Ui) {
        let project = self.session.project().clone();
        // One row more than there are tracks: it holds the Add track button and takes dropped clips.
        let rows = (project.tracks.len() + 1) as f32;
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), rows * TRACK_H), Sense::click_and_drag());
        let lane_left = rect.left() + HEADER_W;

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
        self.handle_mouse(&project, rect, &response);

        let painter = ui.painter_at(rect);
        let text = ui.visuals().text_color();
        for (i, track) in project.tracks.iter().enumerate() {
            let row = Rect::from_min_size(
                pos2(rect.left(), rect.top() + i as f32 * TRACK_H),
                vec2(rect.width(), TRACK_H),
            );
            painter.rect_filled(
                Rect::from_min_max(row.min, pos2(lane_left, row.bottom())),
                0.0,
                ui.visuals().faint_bg_color,
            );
            painter.text(
                row.min + vec2(8.0, 8.0),
                Align2::LEFT_TOP,
                &track.name,
                FontId::proportional(13.0),
                text,
            );
            painter.line_segment(
                [row.left_bottom(), row.right_bottom()],
                Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
            let delete = Rect::from_min_size(pos2(row.left() + 6.0, row.bottom() - 26.0), vec2(56.0, 20.0));
            if ui
                .put(delete, egui::Button::new("Delete").small())
                .on_hover_text("Delete this track")
                .clicked()
            {
                self.run(Command::RemoveTrack { track: track.id });
            }
        }
        let lanes = ui.painter_at(Rect::from_min_max(pos2(lane_left, rect.top()), rect.max));
        let new_row = project.tracks.len();
        let row_rect = |i: usize| {
            Rect::from_min_size(
                pos2(rect.left(), rect.top() + i as f32 * TRACK_H),
                vec2(rect.width(), TRACK_H),
            )
        };
        let dropping_new = matches!(self.drag, Some(Drag::Move { track: None, .. }));
        let add = Rect::from_min_size(row_rect(new_row).min + vec2(6.0, 8.0), vec2(80.0, 20.0));
        if !dropping_new && ui.put(add, egui::Button::new("Add track").small()).clicked() {
            let name = format!("Track {}", project.tracks.len() + 1);
            self.run(Command::AddTrack { name });
        }
        if dropping_new {
            let row = row_rect(new_row).shrink(2.0);
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
                row_rect(0).center(),
                Align2::CENTER_CENTER,
                "Import audio or add a track",
                FontId::proportional(14.0),
                text,
            );
        }
        let mut ids: Vec<Option<TrackId>> = project.tracks.iter().map(|t| Some(t.id)).collect();
        ids.push(None);
        let tracks: Vec<Vec<Clip>> = ids.into_iter().map(|t| self.preview_track(&project, t)).collect();
        let mut on_top = None;
        for (row, clips) in tracks.iter().enumerate() {
            for index in self.stacking(clips).into_iter().rev() {
                let Some(clip) = clips.get(index) else { continue };
                let fades = edge_fades(clips, index);
                let free = free_ends(clips, index);
                let area = self.clip_area(rect, row, clip.start, clip.len);
                if area.right() <= lane_left {
                    continue;
                }
                let dragged = self.drag.as_ref().is_some_and(|d| d.clip() == clip.id);
                if dragged || Some(clip.id) == self.selected {
                    on_top = Some((area, clip.clone(), fades, free));
                    continue;
                }
                self.draw_clip(&lanes, area, lane_left, clip, (fades, free), false);
            }
        }
        if let Some(Drag::TrimStart { clip, .. } | Drag::TrimEnd { clip, .. }) = &self.drag
            && let Some((track, c)) = project.clip(*clip)
            && let Some(row) = project.tracks.iter().position(|t| t.id == track.id)
        {
            // How far the edges can go: the whole audio file.
            let whole = self.clip_area(rect, row, c.start - c.offset, c.source.frames());
            let r = whole.shrink(1.0);
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
        if let Some((area, clip, fades, free)) = on_top {
            self.draw_clip(&lanes, area, lane_left, &clip, (fades, free), true);
        }
        for (row, clips) in tracks.iter().enumerate() {
            self.draw_overlaps(&lanes, rect, row, clips);
        }
        let x = self.x_of(lane_left, self.player.position());
        if x >= lane_left {
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
            );
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

    fn draw_clip(
        &mut self,
        painter: &egui::Painter,
        area: Rect,
        lane_left: f32,
        clip: &Clip,
        (fades, free): ((Fade, Fade), (bool, bool)),
        selected: bool,
    ) {
        let start = clip.start;
        let fill = if selected {
            Color32::from_rgb(52, 92, 140)
        } else {
            Color32::from_rgb(44, 66, 92)
        };
        painter.rect_filled(area, 3.0, fill);
        painter.rect_stroke(area, 3.0, Stroke::new(1.0, Color32::from_gray(150)), StrokeKind::Inside);
        let visible = Rect::from_min_max(pos2(area.left().max(lane_left), area.top()), area.max);

        // Playback uses the first two channels, so those are the ones shown: one band each.
        let shown = clip.source.channels.len().min(2);
        let band = (area.height() - 4.0) / shown.max(1) as f32;
        let first = self.frame_at(lane_left, visible.left()) - start + clip.offset;
        let cols = visible.width().max(0.0) as usize;
        let wave = Stroke::new(1.0, Color32::from_rgb(150, 200, 240));
        for (channel, samples) in clip.source.channels.iter().take(shown).enumerate() {
            let mid = area.top() + 2.0 + band * (channel as f32 + 0.5);
            let half = band / 2.0 - 2.0;
            let key = (Arc::as_ptr(&clip.source) as usize, channel);
            let (_, peaks) = self
                .peaks
                .entry(key)
                .or_insert_with(|| (Arc::downgrade(&clip.source), Peaks::build(samples)));
            for (col, span) in peaks
                .spans(samples, first as f64, self.frames_per_px, 0.0, cols)
                .into_iter()
                .enumerate()
            {
                if let Some((lo, hi)) = span {
                    let x = visible.left() + col as f32 + 0.5;
                    painter.line_segment(
                        [pos2(x, mid - hi * half), pos2(x, mid - lo.min(hi - 0.01) * half)],
                        wave,
                    );
                }
            }
        }
        if shown == 2 {
            let y = area.top() + 2.0 + band;
            painter.line_segment(
                [pos2(visible.left(), y), pos2(visible.right(), y)],
                Stroke::new(1.0, Color32::from_gray(90)),
            );
        }

        // The volume as heard: curve and fades together.
        let mut line: Vec<Pos2> = vec![];
        let mut x = visible.left();
        while x <= visible.right() {
            let rel = (self.frame_at(lane_left, x) - start).clamp(0, clip.len.saturating_sub(1));
            line.push(pos2(x, y_of_amp(area, clip_amp(clip, fades, rel))));
            x += 2.0;
        }
        painter.add(egui::Shape::line(line, Stroke::new(1.5, CURVE)));

        for point in clip.gain.iter().filter(|p| (0..=clip.len).contains(&p.at)) {
            let pos = pos2(
                self.x_of(lane_left, start + point.at),
                y_of_amp(area, db_to_amp(point.db)),
            );
            painter.circle(pos, 4.0, Color32::from_gray(30), Stroke::new(1.5, CURVE));
        }
        // Crossfaded ends have no handle: the overlap is changed by moving the clip.
        let handles = [
            (free.0, start + clip.fade_in.len),
            (free.1, start + clip.len - clip.fade_out.len),
        ];
        for x in handles
            .into_iter()
            .filter(|(f, _)| *f)
            .map(|(_, frame)| self.x_of(lane_left, frame))
        {
            let handle = Rect::from_center_size(pos2(x, area.top() + HANDLE), vec2(HANDLE * 1.5, HANDLE * 1.5));
            painter.rect_filled(handle, 1.0, Color32::from_gray(220));
        }
        // A dark strip with an arrow at each end that has hidden audio: dragging that edge outwards shows it again.
        let (before, after) = clip.hidden();
        let mid = area.center().y;
        let shade = Color32::from_black_alpha(140);
        let arrow = Color32::WHITE;
        if before > 0 && area.left() >= lane_left {
            let x = area.left();
            painter.rect_filled(Rect::from_x_y_ranges(x..=x + 10.0, area.y_range()), 3.0, shade);
            let tip = vec![pos2(x + 2.0, mid), pos2(x + 8.0, mid - 7.0), pos2(x + 8.0, mid + 7.0)];
            painter.add(egui::Shape::convex_polygon(tip, arrow, Stroke::NONE));
        }
        if after > 0 {
            let x = area.right();
            painter.rect_filled(Rect::from_x_y_ranges(x - 10.0..=x, area.y_range()), 3.0, shade);
            let tip = vec![pos2(x - 2.0, mid), pos2(x - 8.0, mid + 7.0), pos2(x - 8.0, mid - 7.0)];
            painter.add(egui::Shape::convex_polygon(tip, arrow, Stroke::NONE));
        }
        painter.text(
            visible.left_top() + vec2(14.0, 2.0),
            Align2::LEFT_TOP,
            &clip.source.name,
            FontId::proportional(11.0),
            Color32::WHITE,
        );
    }

    /// Tints each crossfade and draws both of its fade lines, which the clip on top would hide.
    fn draw_overlaps(&self, painter: &egui::Painter, rect: Rect, row: usize, clips: &[Clip]) {
        let lane_left = rect.left() + HEADER_W;
        for (i, pair) in clips.windows(2).enumerate() {
            let [a, b] = pair else { continue };
            if a.end() <= b.start {
                continue;
            }
            let area = self.clip_area(rect, row, a.start, a.len);
            let x0 = self.x_of(lane_left, b.start);
            let x1 = self.x_of(lane_left, a.end().min(b.end()));
            let zone = Rect::from_min_max(pos2(x0, area.top()), pos2(x1, area.bottom()));
            painter.rect_filled(zone, 0.0, OVERLAP_FILL);
            painter.rect_stroke(zone, 0.0, Stroke::new(1.0, CURVE), StrokeKind::Inside);
            for (index, clip) in [(i, a), (i + 1, b)] {
                let fades = edge_fades(clips, index);
                let mut line = vec![];
                let mut x = zone.left().max(lane_left);
                while x <= zone.right() {
                    let rel = (self.frame_at(lane_left, x) - clip.start).clamp(0, clip.len.saturating_sub(1));
                    line.push(pos2(x, y_of_amp(area, clip_amp(clip, fades, rel))));
                    x += 2.0;
                }
                painter.add(egui::Shape::line(line, Stroke::new(1.5, CURVE)));
            }
        }
    }

    /// Clip indices from the top of the stack down: the selected clip, then later clips over earlier ones.
    pub(crate) fn stacking(&self, clips: &[Clip]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..clips.len()).rev().collect();
        if let Some(pos) = order.iter().position(|&i| clips.get(i).map(|c| c.id) == self.selected) {
            let selected = order.remove(pos);
            order.insert(0, selected);
        }
        order
    }

    /// Time labels above the tracks; clicking moves the play position.
    pub(crate) fn ruler(&mut self, ui: &mut Ui) {
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), RULER_H), Sense::click_and_drag());
        let lane_left = rect.left() + HEADER_W;
        if let Some(pos) = response.interact_pointer_pos()
            && pos.x >= lane_left
        {
            self.player.seek(self.frame_at(lane_left, pos.x));
        }
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
        let x = self.x_of(lane_left, self.player.position());
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.5, Color32::from_rgb(230, 70, 70)),
        );
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
        (self.session.project().len() as f64 + visible / 2.0).max(visible)
    }

    pub(crate) fn clamp_scroll(&mut self, lane_w: f32) {
        let visible = f64::from(lane_w.max(1.0)) * self.frames_per_px;
        let max = (self.scroll_total(lane_w) - visible).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    pub(crate) fn clip_area(&self, rect: Rect, row: usize, start: Frames, len: Frames) -> Rect {
        clip_area(rect, row, start, len, self.scroll, self.frames_per_px)
    }

    pub(crate) fn x_of(&self, lane_left: f32, frame: Frames) -> f32 {
        lane_left + ((frame as f64 - self.scroll) / self.frames_per_px) as f32
    }

    pub(crate) fn frame_at(&self, lane_left: f32, x: f32) -> Frames {
        (self.scroll + f64::from(x - lane_left) * self.frames_per_px).round() as Frames
    }
}

pub(crate) fn clip_area(rect: Rect, row: usize, start: Frames, len: Frames, scroll: f64, frames_per_px: f64) -> Rect {
    let lane_left = rect.left() + HEADER_W;
    let x = |f: Frames| lane_left + ((f as f64 - scroll) / frames_per_px) as f32;
    let top = rect.top() + row as f32 * TRACK_H + 2.0;
    Rect::from_min_max(
        pos2(x(start), top),
        pos2(x(start.saturating_add(len)), top + TRACK_H - 4.0),
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
pub(crate) fn free_ends(clips: &[Clip], i: usize) -> (bool, bool) {
    let overlaps = |a: Option<&Clip>, b: Option<&Clip>| matches!((a, b), (Some(a), Some(b)) if a.end() > b.start);
    let prev = i.checked_sub(1).and_then(|j| clips.get(j));
    (!overlaps(prev, clips.get(i)), !overlaps(clips.get(i), clips.get(i + 1)))
}
