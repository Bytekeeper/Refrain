//! Mouse editing on the timeline: selecting, moving and trimming clips, fades and volume points.

use audioeditor_engine::{
    ChannelMode, Clip, ClipId, Command, Fade, FadeShape, Frames, GainPoint, Project, TrackId, overlap_before,
};
use egui::{Pos2, Rect, pos2};

use crate::timeline::{clip_area, db_of_y, db_to_amp, free_ends, trash_button, y_of_amp};
use crate::{Editor, HANDLE, HEADER_W};

/// What the mouse is dragging. The project only changes when the drag ends, so one drag is one undo step.
pub(crate) enum Drag {
    /// `track` is `None` over the empty row below all tracks: dropping there makes a new track.
    Move {
        clip: ClipId,
        grab: Frames,
        start: Frames,
        track: Option<TrackId>,
    },
    TrimStart {
        clip: ClipId,
        start: Frames,
    },
    TrimEnd {
        clip: ClipId,
        end: Frames,
    },
    FadeIn {
        clip: ClipId,
        len: Frames,
    },
    FadeOut {
        clip: ClipId,
        len: Frames,
    },
    Point {
        clip: ClipId,
        index: usize,
        point: GainPoint,
    },
}

/// What a right-click menu is about.
#[derive(Clone, Copy)]
pub(crate) enum Menu {
    Fade(FadeMenu),
    /// The channels of one clip.
    Clip(ClipId),
    /// The channels of all clips on a track; opened from the track header.
    Track(TrackId),
}

/// The fade whose curve the right-click menu changes.
#[derive(Clone, Copy)]
pub(crate) enum FadeMenu {
    /// The crossfade into the start of this clip.
    Crossfade(ClipId),
    FadeIn(ClipId),
    FadeOut(ClipId),
}

/// What is under the mouse, most specific first.
pub(crate) enum Hit {
    /// The delete button on the selected clip.
    Trash(ClipId),
    Point(ClipId, usize),
    FadeIn(ClipId),
    FadeOut(ClipId),
    TrimStart(ClipId),
    TrimEnd(ClipId),
    Clip(ClipId, TrackId, Frames),
}

impl Drag {
    pub(crate) fn clip(&self) -> ClipId {
        match self {
            Drag::Move { clip, .. }
            | Drag::TrimStart { clip, .. }
            | Drag::TrimEnd { clip, .. }
            | Drag::FadeIn { clip, .. }
            | Drag::FadeOut { clip, .. }
            | Drag::Point { clip, .. } => *clip,
        }
    }
}

impl Hit {
    fn clip(&self) -> ClipId {
        match self {
            Hit::Trash(c)
            | Hit::Point(c, _)
            | Hit::FadeIn(c)
            | Hit::FadeOut(c)
            | Hit::TrimStart(c)
            | Hit::TrimEnd(c)
            | Hit::Clip(c, ..) => *c,
        }
    }
}

impl Editor {
    pub(crate) fn handle_mouse(&mut self, project: &Project, rect: Rect, response: &egui::Response) {
        let lane_left = rect.left() + HEADER_W;
        let trimming = matches!(self.drag, Some(Drag::TrimStart { .. } | Drag::TrimEnd { .. }));
        let on_edge = response.hover_pos().and_then(|p| self.hit(project, rect, p));
        if trimming || matches!(on_edge, Some(Hit::TrimStart(_) | Hit::TrimEnd(_))) {
            response.ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        let Some(pos) = response.interact_pointer_pos() else {
            return;
        };
        let frame = self.frame_at(lane_left, pos.x);
        let hit = self.hit(project, rect, pos);

        if response.clicked() {
            if let Some(Hit::Trash(clip)) = hit {
                self.run(Command::DeleteClip { clip });
                return;
            }
            self.selected = hit.as_ref().map(Hit::clip);
            self.player.seek(frame);
        }
        if response.double_clicked()
            && let Some(Hit::Clip(id, ..)) = hit
            && let Some((track, clip)) = project.clip(id)
        {
            let row = project.tracks.iter().position(|t| t.id == track.id).unwrap_or(0);
            let area = self.clip_area(rect, row, clip.start, clip.len);
            let mut points = clip.gain.clone();
            points.push(GainPoint {
                at: frame - clip.start,
                db: db_of_y(area, pos.y),
            });
            self.run(Command::SetGainCurve { clip: id, points });
        }
        if response.secondary_clicked() {
            self.menu = if pos.x < lane_left {
                project.tracks.get(self.row_at(rect, pos.y)).map(|t| Menu::Track(t.id))
            } else {
                match hit {
                    Some(Hit::FadeIn(clip)) => Some(Menu::Fade(FadeMenu::FadeIn(clip))),
                    Some(Hit::FadeOut(clip)) => Some(Menu::Fade(FadeMenu::FadeOut(clip))),
                    Some(Hit::Point(..)) => None,
                    _ => self
                        .crossfade_at(project, rect, pos)
                        .map(Menu::Fade)
                        .or(hit.as_ref().map(|h| Menu::Clip(h.clip()))),
                }
            };
            if let Some(Hit::Point(id, index)) = hit
                && let Some((_, clip)) = project.clip(id)
            {
                let mut points = clip.gain.clone();
                if index < points.len() {
                    points.remove(index);
                }
                self.run(Command::SetGainCurve { clip: id, points });
            }
        }

        if response.dragged_by(egui::PointerButton::Middle) {
            self.scroll -= f64::from(response.drag_delta().x) * self.frames_per_px;
            self.clamp_scroll(rect.width() - HEADER_W);
        }
        if response.drag_started_by(egui::PointerButton::Primary) {
            // A drag only starts after the mouse moved a little; hit-test where the button went down.
            let origin = response.ctx.input(|i| i.pointer.press_origin()).unwrap_or(pos);
            let hit = self.hit(project, rect, origin);
            let frame = self.frame_at(lane_left, origin.x);
            self.drag = match hit {
                Some(Hit::Point(clip, index)) => project
                    .clip(clip)
                    .and_then(|(_, c)| c.gain.get(index))
                    .map(|&point| Drag::Point { clip, index, point }),
                Some(Hit::FadeIn(clip)) => project.clip(clip).map(|(_, c)| Drag::FadeIn {
                    clip,
                    len: c.fade_in.len,
                }),
                Some(Hit::FadeOut(clip)) => project.clip(clip).map(|(_, c)| Drag::FadeOut {
                    clip,
                    len: c.fade_out.len,
                }),
                Some(Hit::TrimStart(clip)) => project
                    .clip(clip)
                    .map(|(_, c)| Drag::TrimStart { clip, start: c.start }),
                Some(Hit::TrimEnd(clip)) => project.clip(clip).map(|(_, c)| Drag::TrimEnd { clip, end: c.end() }),
                Some(Hit::Trash(_)) => None,
                Some(Hit::Clip(clip, track, start)) => Some(Drag::Move {
                    clip,
                    grab: frame - start,
                    start,
                    track: Some(track),
                }),
                None => None,
            };
            if let Some(clip) = hit.as_ref().map(Hit::clip) {
                self.selected = Some(clip);
            }
        }
        self.update_drag(project, rect, pos, frame);
        if response.drag_stopped_by(egui::PointerButton::Primary)
            && let Some(drag) = self.drag.take()
        {
            self.finish_drag(project, drag);
        }
    }

    fn update_drag(&mut self, project: &Project, rect: Rect, pos: Pos2, frame: Frames) {
        // Row geometry first: it reads the editor, which the drag below borrows.
        let rows: Vec<Rect> = (0..=project.tracks.len()).map(|r| self.row_rect(rect, r)).collect();
        let row_under = self.row_at(rect, pos.y);
        let Some(drag) = &mut self.drag else { return };
        match drag {
            Drag::Move {
                clip,
                grab,
                start,
                track,
            } => {
                // Moving past 0 hides the clip's start; at least one frame stays after 0.
                let len = project.clip(*clip).map_or(1, |(_, c)| c.len);
                *start = (frame - *grab).max(1 - len);
                if let Some(l) = project.loop_end {
                    // In a loop every clip starts inside it.
                    *start = (*start).clamp(0, l - 1);
                }
                *track = project.tracks.get(row_under).map(|t| t.id);
            }
            // Up to the ends of the audio file, and at least one frame of the clip stays after 0.
            Drag::TrimStart { clip, start } => {
                if let Some((_, c)) = project.clip(*clip) {
                    let first = c.start - c.offset;
                    let first = if project.loop_end.is_some() {
                        first.max(0)
                    } else {
                        first
                    };
                    *start = frame.clamp(first, c.end() - 1);
                }
            }
            Drag::TrimEnd { clip, end } => {
                if let Some((_, c)) = project.clip(*clip) {
                    let audio_end = c.start - c.offset + c.source.frames();
                    *end = frame.clamp((c.start + 1).max(1), audio_end.max(1));
                }
            }
            Drag::FadeIn { clip, len } => {
                if let Some((_, c)) = project.clip(*clip) {
                    *len = (frame - c.start).clamp(0, c.len - c.fade_out.len);
                }
            }
            Drag::FadeOut { clip, len } => {
                if let Some((_, c)) = project.clip(*clip) {
                    *len = (c.end() - frame).clamp(0, c.len - c.fade_in.len);
                }
            }
            Drag::Point { clip, point, .. } => {
                let found = project
                    .clip(*clip)
                    .and_then(|(t, c)| Some((project.tracks.iter().position(|x| x.id == t.id)?, c)));
                if let Some((row, c)) = found {
                    let area = clip_area(
                        rows.get(row).copied().unwrap_or(rect),
                        rect.left() + HEADER_W,
                        (c.start, c.len),
                        self.scroll,
                        self.frames_per_px,
                    );
                    point.at = (frame - c.start).clamp(0, c.len);
                    point.db = db_of_y(area, pos.y);
                    self.status = format!("{:.1} dB", point.db);
                    self.status_error = false;
                }
            }
        }
    }

    fn finish_drag(&mut self, project: &Project, drag: Drag) {
        let command = match drag {
            Drag::Move {
                clip,
                start,
                track: Some(track),
                ..
            } => Command::MoveClip { clip, track, start },
            Drag::Move {
                clip,
                start,
                track: None,
                ..
            } => Command::MoveClipToNewTrack { clip, start },
            Drag::TrimStart { clip, start } => {
                let Some((_, c)) = project.clip(clip) else { return };
                Command::TrimClip {
                    clip,
                    start,
                    end: c.end(),
                }
            }
            Drag::TrimEnd { clip, end } => {
                let Some((_, c)) = project.clip(clip) else { return };
                Command::TrimClip {
                    clip,
                    start: c.start,
                    end,
                }
            }
            Drag::FadeIn { clip, len } => {
                let shape = project.clip(clip).map_or(FadeShape::Linear, |(_, c)| c.fade_in.shape);
                Command::SetFadeIn {
                    clip,
                    fade: Fade { len, shape },
                }
            }
            Drag::FadeOut { clip, len } => {
                let shape = project.clip(clip).map_or(FadeShape::Linear, |(_, c)| c.fade_out.shape);
                Command::SetFadeOut {
                    clip,
                    fade: Fade { len, shape },
                }
            }
            Drag::Point { clip, index, point } => {
                let Some((_, c)) = project.clip(clip) else { return };
                let mut points = c.gain.clone();
                if let Some(p) = points.get_mut(index) {
                    *p = point;
                }
                Command::SetGainCurve { clip, points }
            }
        };
        self.run(command);
    }

    /// The crossfade under `pos`, named by the clip it fades into; with a loop also the seam at the start.
    fn crossfade_at(&self, project: &Project, rect: Rect, pos: Pos2) -> Option<FadeMenu> {
        let row = self.row_at(rect, pos.y);
        let clips = &project.tracks.get(row)?.clips;
        let frame = self.frame_at(rect.left() + HEADER_W, pos.x);
        (0..clips.len()).find_map(|i| {
            let clip = clips.get(i)?;
            let len = overlap_before(clips, i, project.loop_end);
            (clip.start <= frame && frame < clip.start.saturating_add(len)).then_some(FadeMenu::Crossfade(clip.id))
        })
    }

    pub(crate) fn menu_ui(&mut self, ui: &mut egui::Ui, menu: Menu) {
        match menu {
            Menu::Fade(fade) => self.fade_menu_ui(ui, fade),
            Menu::Clip(_) | Menu::Track(_) => self.channels_menu_ui(ui, menu),
        }
    }

    /// Which channels a clip, or every clip on a track, plays.
    fn channels_menu_ui(&mut self, ui: &mut egui::Ui, menu: Menu) {
        let project = self.session.project().clone();
        let (title, clips): (&str, Vec<&Clip>) = match menu {
            Menu::Track(id) => (
                "Channels of all clips",
                project.track(id).map(|t| t.clips.iter().collect()).unwrap_or_default(),
            ),
            Menu::Clip(id) => ("Channels", project.clip(id).map(|(_, c)| vec![c]).unwrap_or_default()),
            Menu::Fade(_) => return,
        };
        ui.label(title);
        if clips.is_empty() {
            ui.weak("No clips on this track");
            return;
        }
        if clips.iter().all(|c| c.source.channels.len() < 2) {
            ui.weak("Mono recording: it plays the same on both sides");
            return;
        }
        let first = clips.first().map(|c| c.channels);
        let current = first.filter(|m| clips.iter().all(|c| c.channels == *m));
        let modes = [
            (ChannelMode::Stereo, "Stereo", "Both channels as recorded"),
            (
                ChannelMode::Mono,
                "Mono (mix of both)",
                "Left and right mixed, on both sides",
            ),
            (
                ChannelMode::Left,
                "Left only",
                "The left channel on both sides, for a broken right channel",
            ),
            (
                ChannelMode::Right,
                "Right only",
                "The right channel on both sides, for a broken left channel",
            ),
        ];
        for (mode, name, hint) in modes {
            if ui.radio(current == Some(mode), name).on_hover_text(hint).clicked() {
                let clips = clips.iter().map(|c| c.id).collect();
                self.run(Command::SetChannels { clips, mode });
                ui.close();
            }
        }
    }

    /// The right-click menu for a fade's curve.
    fn fade_menu_ui(&mut self, ui: &mut egui::Ui, menu: FadeMenu) {
        let project = self.session.project().clone();
        let (title, current) = match menu {
            FadeMenu::Crossfade(id) => ("Crossfade curve", project.clip(id).map(|(_, c)| c.crossfade_shape)),
            FadeMenu::FadeIn(id) => ("Fade-in curve", project.clip(id).map(|(_, c)| c.fade_in.shape)),
            FadeMenu::FadeOut(id) => ("Fade-out curve", project.clip(id).map(|(_, c)| c.fade_out.shape)),
        };
        let Some(current) = current else {
            ui.close();
            return;
        };
        ui.label(title);
        let shapes = [
            (
                FadeShape::Linear,
                "Linear",
                "Straight. In a crossfade: best where both parts have a high waveform match",
            ),
            (
                FadeShape::EqualPower,
                "Equal power",
                "Keeps the loudness even. In a crossfade: best for unrelated sound such as rain or noise",
            ),
        ];
        for (shape, name, hint) in shapes {
            if ui.radio(current == shape, name).on_hover_text(hint).clicked() {
                let command = match menu {
                    FadeMenu::Crossfade(clip) => Command::SetCrossfadeShape { clip, shape },
                    FadeMenu::FadeIn(clip) => Command::SetFadeIn {
                        clip,
                        fade: Fade {
                            len: project.clip(clip).map_or(0, |(_, c)| c.fade_in.len),
                            shape,
                        },
                    },
                    FadeMenu::FadeOut(clip) => Command::SetFadeOut {
                        clip,
                        fade: Fade {
                            len: project.clip(clip).map_or(0, |(_, c)| c.fade_out.len),
                            shape,
                        },
                    },
                };
                self.run(command);
                ui.close();
            }
        }
        if let FadeMenu::Crossfade(clip) = menu {
            ui.separator();
            if ui
                .button("Find best match")
                .on_hover_text(
                    "Moves the crossfade by up to 50 ms to where both waveforms match best, and picks the curve. \
                     At the loop seam this moves the loop end.",
                )
                .clicked()
            {
                let range = Frames::from(project.sample_rate) * 50 / 1000;
                if self.run(Command::MatchCrossfade { clip, range }) {
                    self.info("Moved to the best waveform match within 50 ms.".into());
                }
                ui.close();
            }
        }
    }

    fn hit(&self, project: &Project, rect: Rect, pos: Pos2) -> Option<Hit> {
        // The header column holds track controls, never clips, even where a clip starts before 0.
        if pos.x < rect.left() + HEADER_W {
            return None;
        }
        let row = self.row_at(rect, pos.y);
        let track = project.tracks.get(row)?;
        let lane_left = rect.left() + HEADER_W;
        let near = |p: Pos2| p.distance(pos) <= HANDLE + 2.0;
        let order = self.stacking(&track.clips);
        for &i in &order {
            let Some(clip) = track.clips.get(i) else { continue };
            let area = self.clip_area(rect, row, clip.start, clip.len);
            if Some(clip.id) == self.selected && trash_button(area, rect.right()).is_some_and(|b| b.contains(pos)) {
                return Some(Hit::Trash(clip.id));
            }
            let free = free_ends(&track.clips, i, project.loop_end);
            // Points in a trimmed-off part still shape the curve but cannot be grabbed.
            for (index, point) in clip.gain.iter().enumerate() {
                if (0..=clip.len).contains(&point.at)
                    && near(pos2(
                        self.x_of(lane_left, clip.start + point.at),
                        y_of_amp(area, db_to_amp(point.db)),
                    ))
                {
                    return Some(Hit::Point(clip.id, index));
                }
            }
            if free.0
                && near(pos2(
                    self.x_of(lane_left, clip.start + clip.fade_in.len),
                    area.top() + HANDLE,
                ))
            {
                return Some(Hit::FadeIn(clip.id));
            }
            if free.1
                && near(pos2(
                    self.x_of(lane_left, clip.end() - clip.fade_out.len),
                    area.top() + HANDLE,
                ))
            {
                return Some(Hit::FadeOut(clip.id));
            }
            // A narrow clip keeps room in the middle for moving it.
            let grip = HANDLE.min(area.width() / 4.0);
            let inside = (area.top()..=area.bottom()).contains(&pos.y);
            if inside && (area.left() - 2.0..=area.left() + grip).contains(&pos.x) {
                return Some(Hit::TrimStart(clip.id));
            }
            if inside && (area.right() - grip..=area.right() + 2.0).contains(&pos.x) {
                return Some(Hit::TrimEnd(clip.id));
            }
        }
        let frame = self.frame_at(lane_left, pos.x);
        let clip = order
            .iter()
            .filter_map(|&i| track.clips.get(i))
            .find(|c| c.start <= frame && frame < c.end())?;
        Some(Hit::Clip(clip.id, track.id, clip.start))
    }
}
