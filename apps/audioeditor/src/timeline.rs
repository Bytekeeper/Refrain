//! Drawing the timeline: tracks, clips, crossfades, the ruler and the scrollbar.

use std::sync::Arc;

use audioeditor_engine::{
    Clip, Fade, Frames, MAX_DB, MIN_DB, Project, TrackId, clip_amp, edge_fades, overlap_after, overlap_before,
    similarity,
};
use craft_audio::peaks::Peaks;
use egui::{Align2, Color32, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::mouse::Drag;
use crate::{Command, Editor, HANDLE, HEADER_W, TRACK_H};

const RULER_H: f32 = 22.0;
pub(crate) const SCROLLBAR_H: f32 = 12.0;
pub(crate) const CURVE: Color32 = Color32::from_rgb(255, 200, 80);
const LOOP: Color32 = Color32::from_rgb(90, 200, 120);
const WAVE: Color32 = Color32::from_rgb(150, 200, 240);
/// The later clip's waveform where two clips overlap.
const WAVE_LATER: Color32 = Color32::from_rgb(245, 175, 110);

/// How a clip is drawn.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    Normal,
    Selected,
    /// The part of a clip past the loop end, shown again at the start where it plays.
    Wrapped,
}

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
        look: Look,
    ) {
        let start = clip.start;
        let fill = match look {
            Look::Normal => Color32::from_rgb(44, 66, 92),
            Look::Selected => Color32::from_rgb(52, 92, 140),
            Look::Wrapped => Color32::from_rgb(40, 70, 58),
        };
        painter.rect_filled(area, 3.0, fill);
        if look == Look::Wrapped {
            let r = area.shrink(0.5).intersect(painter.clip_rect().expand(2.0));
            let corners = [
                r.left_top(),
                r.right_top(),
                r.right_bottom(),
                r.left_bottom(),
                r.left_top(),
            ];
            painter.extend(egui::Shape::dashed_line(&corners, Stroke::new(1.0, LOOP), 6.0, 4.0));
        } else {
            painter.rect_stroke(area, 3.0, Stroke::new(1.0, Color32::from_gray(150)), StrokeKind::Inside);
        }
        // Only the part on screen: deep in, a clip can be millions of pixels wide.
        let view = painter.clip_rect();
        let visible = Rect::from_x_y_ranges(
            area.left().max(view.left())..=area.right().min(view.right()),
            area.y_range(),
        );

        // Playback uses the first two channels, so those are the ones shown: one band each.
        let shown = clip.source.channels.len().min(2);
        let band = (area.height() - 4.0) / shown.max(1) as f32;
        for channel in 0..shown {
            let top = area.top() + 2.0 + band * channel as f32;
            let rows = Rect::from_x_y_ranges(visible.x_range(), top..=top + band);
            self.draw_wave(painter, lane_left, clip, channel, rows, WAVE);
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

        if look == Look::Wrapped {
            painter.with_clip_rect(visible.intersect(painter.clip_rect())).text(
                visible.left_top() + vec2(14.0, 2.0),
                Align2::LEFT_TOP,
                format!("from loop end: {}", clip.source.name),
                FontId::proportional(11.0),
                LOOP,
            );
            return;
        }
        self.draw_points(painter, area, lane_left, clip, 0..=clip.len);
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

    /// Shows each crossfade in its own way: both clips' waveforms channel by channel (left of both, then
    /// right of both), so peaks can be lined up, both fade lines, and how alike the two sound.
    /// `wrapped` are the parts from past the loop end, which crossfade with the start of the track.
    fn draw_overlaps(
        &mut self,
        painter: &egui::Painter,
        rect: Rect,
        row: usize,
        clips: &[Clip],
        wrap: Option<Frames>,
        wrapped: &[(Clip, (Fade, Fade))],
    ) {
        let lane_left = rect.left() + HEADER_W;
        let view = painter.clip_rect();
        // Wrapped parts first, so at the same start they count as the earlier clip.
        let mut parts: Vec<(&Clip, (Fade, Fade), bool)> = wrapped.iter().map(|(c, f)| (c, *f, true)).collect();
        parts.extend(
            clips
                .iter()
                .enumerate()
                .map(|(i, c)| (c, edge_fades(clips, i, wrap), false)),
        );
        parts.sort_by_key(|(c, ..)| c.start);
        for pair in parts.windows(2) {
            let [a, b] = pair else { continue };
            let (from, to) = (b.0.start, a.0.end().min(b.0.end()));
            if to <= from {
                continue;
            }
            let area = self.clip_area(rect, row, a.0.start, a.0.len);
            let zone = Rect::from_x_y_ranges(self.x_of(lane_left, from)..=self.x_of(lane_left, to), area.y_range());
            let shown = Rect::from_x_y_ranges(
                zone.left().max(view.left())..=zone.right().min(view.right()),
                zone.y_range(),
            );
            if shown.width() <= 0.0 {
                continue;
            }
            painter.rect_filled(zone, 0.0, Color32::from_rgb(34, 44, 56));

            let stereo = a.0.source.channels.len() >= 2 || b.0.source.channels.len() >= 2;
            let names: &[&str] = if stereo { &["L1", "L2", "R1", "R2"] } else { &["1", "2"] };
            let band = (zone.height() - 4.0) / names.len() as f32;
            for (i, name) in names.iter().enumerate() {
                let (clip, color) = if i % 2 == 0 { (a.0, WAVE) } else { (b.0, WAVE_LATER) };
                let channel = (i / 2).min(clip.source.channels.len().saturating_sub(1));
                let top = zone.top() + 2.0 + band * i as f32;
                let rows = Rect::from_x_y_ranges(shown.x_range(), top..=top + band);
                self.draw_wave(painter, lane_left, clip, channel, rows, color);
                if i % 2 == 1 && i + 1 < names.len() {
                    let y = top + band;
                    painter.line_segment(
                        [pos2(shown.left(), y), pos2(shown.right(), y)],
                        Stroke::new(1.0, Color32::from_gray(90)),
                    );
                }
                if zone.width() > 40.0 {
                    painter.text(
                        pos2(shown.left() + 3.0, top + 1.0),
                        Align2::LEFT_TOP,
                        *name,
                        FontId::monospace(9.0),
                        color,
                    );
                }
            }
            painter.rect_stroke(zone, 0.0, Stroke::new(1.0, CURVE), StrokeKind::Inside);

            for &(clip, fades, is_wrapped) in [a, b] {
                let mut line = vec![];
                let mut x = shown.left();
                while x <= shown.right() {
                    let rel = (self.frame_at(lane_left, x) - clip.start).clamp(0, clip.len.saturating_sub(1));
                    line.push(pos2(x, y_of_amp(area, clip_amp(clip, fades, rel))));
                    x += 2.0;
                }
                painter.add(egui::Shape::line(line, Stroke::new(1.5, CURVE)));
                if !is_wrapped {
                    self.draw_points(painter, area, lane_left, clip, from - clip.start..=to - clip.start);
                }
            }

            if let Some(alike) = similarity((a.0, a.1), (b.0, b.1), from, to) {
                let percent = (alike * 100.0).round() as i32;
                let at = pos2(shown.center().x, zone.bottom() - 3.0);
                // The longest text that fits the overlap.
                let fitting = ["waveform match ", "match ", ""].into_iter().find_map(|prefix| {
                    let galley = painter.layout_no_wrap(
                        format!("{prefix}{percent}%"),
                        FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                    let size = galley.size();
                    let label = Rect::from_min_size(at - vec2(size.x / 2.0, size.y), size).expand(2.0);
                    (label.width() < zone.width()).then_some((label, galley))
                });
                if let Some((label, galley)) = fitting {
                    painter.rect_filled(label, 3.0, Color32::from_black_alpha(170));
                    painter.galley(label.min + vec2(2.0, 2.0), galley, Color32::WHITE);
                }
            }
        }
    }

    /// One channel of `clip` as seen in `rows`, which gives the x range and the height.
    fn draw_wave(
        &mut self,
        painter: &egui::Painter,
        lane_left: f32,
        clip: &Clip,
        channel: usize,
        rows: Rect,
        color: Color32,
    ) {
        let Some(samples) = clip.source.channels.get(channel) else {
            return;
        };
        let mid = rows.center().y;
        let half = rows.height() / 2.0 - 2.0;
        let first = self.frame_at(lane_left, rows.left()) - clip.start + clip.offset;
        let cols = rows.width().max(0.0) as usize;
        let key = (Arc::as_ptr(&clip.source) as usize, channel);
        let (_, peaks) = self
            .peaks
            .entry(key)
            .or_insert_with(|| (Arc::downgrade(&clip.source), Peaks::build(samples)));
        let spans = peaks.spans(samples, first as f64, self.frames_per_px, 0.0, cols);
        let x_of_col = |col: usize| rows.left() + col as f32 + 0.5;
        if self.frames_per_px < 4.0 {
            // Zoomed in, a column holds one or two samples: one line through them shows the wave's shape.
            let mut line = vec![];
            for (col, span) in spans.into_iter().enumerate() {
                match span {
                    Some((lo, hi)) => line.push(pos2(x_of_col(col), mid - (lo + hi) / 2.0 * half)),
                    None => {
                        painter.add(egui::Shape::line(std::mem::take(&mut line), Stroke::new(1.0, color)));
                    }
                }
            }
            painter.add(egui::Shape::line(line, Stroke::new(1.0, color)));
            return;
        }
        for (col, span) in spans.into_iter().enumerate() {
            if let Some((lo, hi)) = span {
                let x = x_of_col(col);
                painter.line_segment(
                    [pos2(x, mid - hi * half), pos2(x, mid - lo.min(hi - 0.01) * half)],
                    Stroke::new(1.0, color),
                );
            }
        }
    }

    /// The volume points of `clip` that lie in `range` (frames from the clip's start).
    fn draw_points(
        &self,
        painter: &egui::Painter,
        area: Rect,
        lane_left: f32,
        clip: &Clip,
        range: std::ops::RangeInclusive<Frames>,
    ) {
        let visible = 0..=clip.len;
        for point in clip
            .gain
            .iter()
            .filter(|p| range.contains(&p.at) && visible.contains(&p.at))
        {
            let pos = pos2(
                self.x_of(lane_left, clip.start + point.at),
                y_of_amp(area, db_to_amp(point.db)),
            );
            painter.circle(pos, 4.0, Color32::from_gray(30), Stroke::new(1.5, CURVE));
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
                self.loop_drag = Some(frame.max(1));
            } else if pos.x >= lane_left && !on_flag {
                self.player.seek(frame);
            }
        }
        if response.drag_stopped()
            && let Some(at) = self.loop_drag.take()
        {
            self.run(Command::SetLoopEnd { at: Some(at) });
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
