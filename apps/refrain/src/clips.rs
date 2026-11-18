//! Drawing one clip: waveform, volume curve, fades, title; and the zone where two clips overlap.

use std::sync::Arc;

use craft_audio::peaks::Peaks;
use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};
use refrain_engine::{ChannelMode, Clip, Fade, FadeShape, Frames, Source, clip_amp, edge_fades, similarity};

use crate::timeline::{CURVE, LOOP, db_to_amp, y_of_amp};
use crate::{Editor, HANDLE, HEADER_W, Wave};

const WAVE: Color32 = Color32::from_rgb(150, 200, 240);
/// The later clip's waveform where two clips overlap.
const WAVE_LATER: Color32 = Color32::from_rgb(245, 175, 110);
/// Generated sound is rose instead of blue, and has an icon for those who cannot tell the colours apart.
const GENERATED_WAVE: Color32 = Color32::from_rgb(240, 175, 215);

/// How a clip is drawn.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Look {
    Normal,
    Selected,
    /// The part of a clip past the loop end, shown again at the start where it plays.
    Wrapped,
}

impl Editor {
    pub(crate) fn draw_clip(
        &mut self,
        painter: &egui::Painter,
        area: Rect,
        lane_left: f32,
        clip: &Clip,
        (fades, free): ((Fade, Fade), (bool, bool)),
        look: Look,
    ) {
        let start = clip.start;
        let generated = clip.source.generated.is_some();
        let fill = match (look, generated) {
            (Look::Normal, false) => Color32::from_rgb(44, 66, 92),
            (Look::Selected, false) => Color32::from_rgb(52, 92, 140),
            (Look::Normal, true) => Color32::from_rgb(84, 48, 76),
            (Look::Selected, true) => Color32::from_rgb(122, 62, 110),
            (Look::Wrapped, _) => Color32::from_rgb(40, 70, 58),
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
        let bands = shown_channels(clip);
        let band = (area.height() - 4.0) / bands.len() as f32;
        for (i, &channel) in bands.iter().enumerate() {
            let top = area.top() + 2.0 + band * i as f32;
            let rows = Rect::from_x_y_ranges(visible.x_range(), top..=top + band);
            let color = if generated { GENERATED_WAVE } else { WAVE };
            self.draw_wave(painter, lane_left, clip, channel, rows, color);
        }
        if bands.len() == 2 {
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

        let title = visible.left_top() + vec2(14.0, 2.0);
        let title = if generated {
            generated_icon(painter, title)
        } else {
            title
        };
        if look == Look::Wrapped {
            painter.with_clip_rect(visible.intersect(painter.clip_rect())).text(
                title,
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
            title,
            Align2::LEFT_TOP,
            clip_label(clip),
            FontId::proportional(11.0),
            Color32::WHITE,
        );
    }

    /// Shows each crossfade in its own way: both clips' waveforms channel by channel (left of both, then
    /// right of both), so peaks can be lined up, both fade lines, and how alike the two sound.
    /// `wrapped` are the parts from past the loop end, which crossfade with the start of the track.
    pub(crate) fn draw_overlaps(
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

            let (channels_a, channels_b) = (shown_channels(a.0), shown_channels(b.0));
            let stereo = channels_a.len() == 2 || channels_b.len() == 2;
            let names: &[&str] = if stereo { &["L1", "L2", "R1", "R2"] } else { &["1", "2"] };
            let band = (zone.height() - 4.0) / names.len() as f32;
            for (i, name) in names.iter().enumerate() {
                let (clip, color, channels) = if i % 2 == 0 {
                    (a.0, WAVE, &channels_a)
                } else {
                    (b.0, WAVE_LATER, &channels_b)
                };
                // A clip shown in one band shows that band for both sides.
                let index = (i / 2).min(channels.len().saturating_sub(1));
                let channel = channels.get(index).copied().unwrap_or(Some(0));
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
            let curve = match b.0.crossfade_shape {
                FadeShape::Linear => "linear",
                FadeShape::EqualPower => "equal power",
            };
            let galley = painter.layout_no_wrap(curve.into(), FontId::proportional(10.0), CURVE);
            if galley.size().x + 40.0 < zone.width() {
                painter.galley(
                    pos2(shown.center().x - galley.size().x / 2.0, zone.top() + 2.0),
                    galley,
                    CURVE,
                );
            }

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
        channel: Option<usize>,
        rows: Rect,
        color: Color32,
    ) {
        let mid = rows.center().y;
        let half = rows.height() / 2.0 - 2.0;
        let first = self.frame_at(lane_left, rows.left()) - clip.start + clip.offset;
        let cols = rows.width().max(0.0) as usize;
        let source = &clip.source;
        let wave = self
            .peaks
            .entry((Arc::as_ptr(source) as usize, channel.unwrap_or(usize::MAX)))
            .or_insert_with(|| {
                let mixed = if channel.is_none() { mixed(source) } else { vec![] };
                let samples = channel.map_or(mixed.as_slice(), |c| source.channels.get(c).map_or(&[], Vec::as_slice));
                Wave {
                    source: Arc::downgrade(source),
                    peaks: Peaks::build(samples),
                    mixed,
                }
            });
        let samples = channel.map_or(wave.mixed.as_slice(), |c| {
            source.channels.get(c).map_or(&[], Vec::as_slice)
        });
        let spans = wave.peaks.spans(samples, first as f64, self.frames_per_px, 0.0, cols);
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
                // At least a pixel high: thinner segments are drawn so faint that a smooth wave falls apart into dots.
                let top = mid - hi * half;
                let bottom = (mid - lo * half).max(top + 1.0);
                painter.line_segment([pos2(x, top), pos2(x, bottom)], Stroke::new(1.0, color));
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
}

/// The delete button in the bottom right corner of the selected clip, kept on screen; none on a narrow clip.
pub(crate) fn trash_button(area: Rect, view_right: f32) -> Option<Rect> {
    let right = area.right().min(view_right) - 4.0;
    let button = Rect::from_min_size(pos2(right - 22.0, area.bottom() - 26.0), vec2(22.0, 22.0));
    (button.left() > area.left() + 30.0).then_some(button)
}

/// A trash can drawn from lines: lid with a handle, and a slightly narrowing bin with grooves.
pub(crate) fn draw_trash_can(painter: &egui::Painter, r: Rect, stroke: Stroke) {
    let lid = r.top() + r.height() * 0.2;
    painter.line_segment([pos2(r.left(), lid), pos2(r.right(), lid)], stroke);
    let handle = vec![
        pos2(r.center().x - r.width() * 0.2, lid),
        pos2(r.center().x - r.width() * 0.2, r.top()),
        pos2(r.center().x + r.width() * 0.2, r.top()),
        pos2(r.center().x + r.width() * 0.2, lid),
    ];
    painter.add(egui::Shape::line(handle, stroke));
    let inset = r.width() * 0.12;
    let bin = vec![
        pos2(r.left() + inset, lid),
        pos2(r.left() + inset * 2.0, r.bottom()),
        pos2(r.right() - inset * 2.0, r.bottom()),
        pos2(r.right() - inset, lid),
    ];
    painter.add(egui::Shape::line(bin, stroke));
    let groove = Stroke::new(1.0, stroke.color);
    for x in [0.4, 0.6] {
        let x = r.left() + r.width() * x;
        painter.line_segment([pos2(x, lid + 3.0), pos2(x, r.bottom() - 3.0)], groove);
    }
}

/// The channels a clip shows, as it plays them; `None` is the mix of left and right.
fn shown_channels(clip: &Clip) -> Vec<Option<usize>> {
    if clip.source.channels.len() < 2 {
        return vec![Some(0)];
    }
    match clip.channels {
        ChannelMode::Stereo => vec![Some(0), Some(1)],
        ChannelMode::Mono => vec![None],
        ChannelMode::Left => vec![Some(0)],
        ChannelMode::Right => vec![Some(1)],
    }
}

fn mixed(source: &Source) -> Vec<f32> {
    match source.channels.as_slice() {
        [left, right, ..] => left.iter().zip(right).map(|(l, r)| (l + r) * 0.5).collect(),
        [only] => only.clone(),
        [] => vec![],
    }
}

/// The clip's name, its speed when changed, and which channels it plays when that is not simply stereo.
fn clip_label(clip: &Clip) -> String {
    let mut label = clip.source.name.clone();
    if clip.keeps_pitch() {
        label += &format!(" · {:.0} %, same pitch", clip.speed() * 100.0);
    } else if clip.sped.is_some() {
        label += &format!(" · {}", speed_text(clip.speed()));
    }
    if clip.source.channels.len() > 1 {
        label += match clip.channels {
            ChannelMode::Stereo => "",
            ChannelMode::Mono => " · mono",
            ChannelMode::Left => " · left only",
            ChannelMode::Right => " · right only",
        };
    }
    label
}

/// "112 % (+2 st)": the speed, and how far that moves the pitch.
pub(crate) fn speed_text(speed: f64) -> String {
    let semitones = 12.0 * speed.log2();
    let pitch = if (semitones - semitones.round()).abs() < 0.05 {
        format!("{:+.0}", semitones.round())
    } else {
        format!("{semitones:+.1}")
    };
    format!("{:.0} % ({pitch} st)", speed * 100.0)
}

/// A small wave before a generated clip's title. Returns where the title goes after it.
fn generated_icon(painter: &egui::Painter, at: Pos2) -> Pos2 {
    let (width, middle) = (12.0, at.y + 7.0);
    let wave: Vec<Pos2> = (0..=12)
        .map(|i| {
            let t = i as f32 / 12.0;
            pos2(at.x + t * width, middle - 3.5 * (t * std::f32::consts::TAU).sin())
        })
        .collect();
    painter.add(egui::Shape::line(wave, Stroke::new(1.5, Color32::WHITE)));
    at + vec2(width + 4.0, 0.0)
}
