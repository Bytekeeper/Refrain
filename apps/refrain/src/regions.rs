//! Region lanes between the ruler and the level row: named parts of the mix, each exported to a file of its
//! own into its lane's folder. Drawing, editing with the mouse, and the menus of a lane and a region.

use std::path::Path;

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};
use refrain_engine::{Command, ExportPart, Frames, LaneId, Project, RangeId};

use crate::lanes::LANE_H;
use crate::{Editor, HANDLE, HEADER_W};

const REGION: Color32 = Color32::from_rgb(90, 200, 170);

/// A region being drawn, moved or resized. The project only changes on release, as one undo step.
pub(crate) struct RegionDrag {
    lane: LaneId,
    /// `None` while a new region is drawn.
    pub(crate) region: Option<RangeId>,
    kind: DragKind,
    start: Frames,
    end: Frames,
}

#[derive(Clone, Copy)]
enum DragKind {
    New { anchor: Frames },
    Move { grab: Frames, len: Frames },
    Start,
    End,
}

/// What the open region menu is about: a lane, and the region in it that was clicked.
pub(crate) type RegionMenu = (LaneId, Option<RangeId>);

impl Editor {
    /// The lanes as one strip; nothing when the project has none.
    pub(crate) fn region_lanes(&mut self, ui: &mut Ui) {
        let project = self.session.project().clone();
        if project.region_lanes.is_empty() {
            return;
        }
        let height = project.region_lanes.len() as f32 * LANE_H;
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
        self.region_mouse(&project, rect, &response);
        if let Some(menu) = self.region_menu
            // A click in the name field must not close it; the buttons close it themselves.
            && egui::Popup::context_menu(&response)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| self.region_menu_ui(ui, menu))
                .is_none()
        {
            self.region_menu = None;
            self.commit_region_name();
        }
        // A command above may have changed the project.
        let project = self.session.project().clone();
        self.draw_region_lanes(ui, rect, &project);
    }

    fn row_of(rect: Rect, project: &Project, y: f32) -> Option<(LaneId, Rect)> {
        let i = ((y - rect.top()) / LANE_H).floor();
        let lane = project.region_lanes.get(usize::try_from(i as i64).ok()?)?;
        let row = Rect::from_min_size(pos2(rect.left(), rect.top() + i * LANE_H), vec2(rect.width(), LANE_H));
        Some((lane.id, row))
    }

    /// The edges as shown, with the drag applied.
    fn shown_region(&self, id: RangeId, start: Frames, end: Frames) -> (Frames, Frames) {
        match &self.region_drag {
            Some(d) if d.region == Some(id) => (d.start, d.end),
            _ => (start, end),
        }
    }

    /// The region under `pos` in `lane`, and which part: its start, its end or inside.
    fn region_at(&self, project: &Project, lane: LaneId, lane_left: f32, pos: Pos2) -> Option<(RangeId, DragKind)> {
        let lane = project.region_lane(lane)?;
        lane.regions.iter().find_map(|r| {
            let (start, end) = self.shown_region(r.id, r.start, r.end);
            self.range_spans(lane_left, start, end)
                .into_iter()
                .find_map(|(x0, x1)| {
                    if (pos.x - x0).abs() <= HANDLE {
                        Some((r.id, DragKind::Start))
                    } else if (pos.x - x1).abs() <= HANDLE {
                        Some((r.id, DragKind::End))
                    } else {
                        (pos.x > x0 && pos.x < x1).then_some((r.id, DragKind::Move { grab: 0, len: 0 }))
                    }
                })
        })
    }

    fn region_mouse(&mut self, project: &Project, rect: Rect, response: &egui::Response) {
        let lane_left = rect.left() + HEADER_W;
        if let Some(hover) = response.hover_pos()
            && let Some((lane, _)) = Self::row_of(rect, project, hover.y)
            && hover.x >= lane_left
            && matches!(
                self.region_at(project, lane, lane_left, hover),
                Some((_, DragKind::Start | DragKind::End))
            )
        {
            response.ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if let Some(hover) = response.hover_pos()
            && let Some((lane, _)) = Self::row_of(rect, project, hover.y)
        {
            let hint = match (hover.x >= lane_left)
                .then(|| self.region_at(project, lane, lane_left, hover))
                .flatten()
            {
                Some((_, DragKind::Start | DragKind::End)) => "Drag: resize the range",
                Some(_) => "Export range · click: play from its start · drag: move · right-click: name, export",
                None => "Export lane · drag: add a range, exported as a file of its own · right-click: folder, format",
            };
            self.hint(hint);
        }
        let Some(pos) = response.interact_pointer_pos() else {
            return;
        };
        let frame = self.frame_at(lane_left, pos.x);
        if response.secondary_clicked()
            && let Some((lane, _)) = Self::row_of(rect, project, pos.y)
        {
            let region = (pos.x >= lane_left)
                .then(|| self.region_at(project, lane, lane_left, pos))
                .flatten()
                .map(|(id, _)| id);
            self.open_region_menu(project, lane, region);
            return;
        }
        if response.clicked()
            && let Some((lane, _)) = Self::row_of(rect, project, pos.y)
        {
            if pos.x < lane_left {
                return;
            }
            // Selecting a region moves the play position to its start, to hear it from there.
            self.selected_range = self.region_at(project, lane, lane_left, pos).map(|(id, _)| id);
            if let Some((_, region)) = self.selected_range.and_then(|id| project.region(id)) {
                self.selected = None;
                self.player.seek(region.start);
            }
            return;
        }
        let origin = response.ctx.input(|i| i.pointer.press_origin());
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(origin) = origin.filter(|o| o.x >= lane_left)
            && let Some((lane, _)) = Self::row_of(rect, project, origin.y)
        {
            let at = self.frame_at(lane_left, origin.x);
            let hit = self.region_at(project, lane, lane_left, origin);
            let region = hit.and_then(|(id, _)| project.region(id)).map(|(_, r)| r);
            self.region_drag = Some(match (hit, region) {
                (Some((id, kind)), Some(r)) => RegionDrag {
                    lane,
                    region: Some(id),
                    kind: match kind {
                        DragKind::Move { .. } => DragKind::Move {
                            grab: at - r.start,
                            len: r.end - r.start,
                        },
                        other => other,
                    },
                    start: r.start,
                    end: r.end,
                },
                _ => RegionDrag {
                    lane,
                    region: None,
                    kind: DragKind::New { anchor: at },
                    start: at,
                    end: at,
                },
            });
            self.selected_range = self.region_drag.as_ref().and_then(|d| d.region);
            self.selected = None;
        }
        let shift = response.ctx.input(|i| i.modifiers.shift);
        let snap = self.snap_targets(project, None, shift);
        let mut guide = None;
        if let Some(drag) = &mut self.region_drag {
            match drag.kind {
                DragKind::New { anchor } => {
                    let to = snap.apply(frame, &mut guide).max(0);
                    (drag.start, drag.end) = (anchor.min(to), anchor.max(to));
                }
                DragKind::Move { grab, len } => {
                    let start = frame - grab;
                    let snapped = snap.apply(start, &mut guide);
                    let start = if snapped != start {
                        snapped
                    } else {
                        snap.apply(start + len, &mut guide) - len
                    };
                    (drag.start, drag.end) = (start.max(0), start.max(0) + len);
                }
                DragKind::Start => drag.start = snap.apply(frame, &mut guide).clamp(0, drag.end - 1),
                DragKind::End => drag.end = snap.apply(frame, &mut guide).max(drag.start + 1),
            }
        }
        if guide.is_some() {
            self.snap_guide = guide;
        }
        if response.drag_stopped()
            && let Some(drag) = self.region_drag.take()
        {
            let (start, end) = (drag.start, drag.end);
            match drag.region {
                Some(region) => {
                    let old = project.region(region).map(|(_, r)| (r.start, r.end));
                    if old != Some((start, end)) {
                        self.run(Command::SetRegion { region, start, end });
                    }
                }
                None if end > start
                    && self.run(Command::AddRegion {
                        lane: drag.lane,
                        start,
                        end,
                    }) =>
                {
                    let lane = self.session.project().region_lane(drag.lane);
                    let added = lane.and_then(|l| l.regions.iter().find(|r| r.start == start));
                    self.selected_range = added.map(|r| r.id);
                }
                None => {}
            }
        }
    }

    fn open_region_menu(&mut self, project: &Project, lane: LaneId, region: Option<RangeId>) {
        self.region_menu = Some((lane, region));
        self.region_name = region
            .and_then(|id| project.region(id))
            .map(|(_, r)| (r.id, r.name.clone()));
    }

    /// Renames the region whose name was typed in its menu, if it changed.
    fn commit_region_name(&mut self) {
        let Some((region, name)) = self.region_name.take() else {
            return;
        };
        let old = self.session.project().region(region).map(|(_, r)| r.name.clone());
        if old.is_some_and(|old| old != name.trim()) {
            self.run(Command::RenameRegion { region, name });
        }
    }

    fn region_menu_ui(&mut self, ui: &mut Ui, (lane, region): RegionMenu) {
        let project = self.session.project().clone();
        let Some(lane) = project.region_lane(lane) else {
            ui.close();
            return;
        };
        if let Some((_, r)) = region.and_then(|id| project.region(id)) {
            ui.horizontal(|ui| {
                let label = ui.label("Name");
                if let Some((_, text)) = &mut self.region_name {
                    let edit = ui.text_edit_singleline(text).labelled_by(label.id);
                    if edit.lost_focus() {
                        self.commit_region_name();
                        self.region_name = Some((
                            r.id,
                            self.session
                                .project()
                                .region(r.id)
                                .map_or_else(String::new, |(_, r)| r.name.clone()),
                        ));
                    }
                }
            })
            .response
            .on_hover_text("The file name; a / puts it in a subfolder");
            match lane.file_of(r) {
                Some(file) => ui.weak(file.display().to_string()),
                None => ui.weak("Choose a folder for the lane to export it"),
            };
            if ui
                .add_enabled(lane.folder.is_some(), egui::Button::new("Export this range"))
                .clicked()
            {
                self.write_export(&[ExportPart::Region(r.id)]);
                ui.close();
            }
            if ui.button("Remove range").clicked() {
                self.run(Command::RemoveRegion { region: r.id });
                ui.close();
            }
            ui.separator();
        }
        ui.horizontal(|ui| {
            ui.label("Folder");
            match lane.folder.as_deref() {
                Some(folder) => {
                    let name = folder.file_name().unwrap_or(folder.as_os_str()).to_string_lossy();
                    ui.label(name).on_hover_text(folder.display().to_string());
                }
                None => {
                    ui.weak("none chosen");
                }
            }
            if ui.button("Choose…").clicked() {
                let mut dialog = rfd::FileDialog::new();
                if let Some(dir) = lane.folder.as_deref().or(self.file.as_deref().and_then(Path::parent)) {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(folder) = dialog.pick_folder() {
                    self.run(Command::SetRegionLane {
                        lane: lane.id,
                        folder: Some(folder),
                        settings: lane.settings.clone(),
                    });
                }
            }
        });
        let mut settings = lane.settings.clone();
        egui::Grid::new(("region lane", lane.id.0))
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                crate::export::settings_rows(ui, &mut settings, project.sample_rate)
            });
        if settings != lane.settings {
            self.run(Command::SetRegionLane {
                lane: lane.id,
                folder: lane.folder.clone(),
                settings,
            });
        }
        let parts: Vec<ExportPart> = lane.regions.iter().map(|r| ExportPart::Region(r.id)).collect();
        let export = egui::Button::new(format!("Export all ranges of the lane ({})", parts.len()));
        if ui
            .add_enabled(lane.folder.is_some() && !parts.is_empty(), export)
            .clicked()
        {
            self.write_export(&parts);
            ui.close();
        }
        ui.separator();
        if ui.button("Add export lane").clicked() {
            self.run(Command::AddRegionLane);
            ui.close();
        }
        if ui.button("Remove export lane").clicked() {
            self.run(Command::RemoveRegionLane { lane: lane.id });
            ui.close();
        }
    }

    fn draw_region_lanes(&self, ui: &Ui, rect: Rect, project: &Project) {
        let lane_left = rect.left() + HEADER_W;
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals();
        for (i, lane) in project.region_lanes.iter().enumerate() {
            let row = Rect::from_min_size(
                pos2(rect.left(), rect.top() + i as f32 * LANE_H),
                vec2(rect.width(), LANE_H),
            );
            let header = Rect::from_min_max(row.min, pos2(lane_left, row.bottom()));
            painter.rect_filled(header, 0.0, visuals.extreme_bg_color);
            painter.rect_filled(
                row.with_min_x(lane_left),
                0.0,
                visuals.extreme_bg_color.gamma_multiply(0.8),
            );
            painter.line_segment(
                [row.left_top(), row.right_top()],
                Stroke::new(0.5, visuals.weak_text_color().gamma_multiply(0.5)),
            );
            let (title, color) = match lane.folder.as_deref() {
                Some(folder) => {
                    let name = folder.file_name().unwrap_or(folder.as_os_str()).to_string_lossy();
                    (format!("Export: {name}"), visuals.text_color())
                }
                None => ("Export: no folder".into(), visuals.weak_text_color()),
            };
            painter.with_clip_rect(header.shrink(2.0)).text(
                header.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                title,
                FontId::proportional(12.0),
                color,
            );

            let body = row.with_min_x(lane_left).shrink2(vec2(0.0, 3.0));
            let painter = painter.with_clip_rect(row.with_min_x(lane_left));
            let mut shown: Vec<(Option<RangeId>, Frames, Frames, &str)> = lane
                .regions
                .iter()
                .map(|r| {
                    let (start, end) = self.shown_region(r.id, r.start, r.end);
                    (Some(r.id), start, end, r.name.as_str())
                })
                .collect();
            if let Some(d) = &self.region_drag
                && d.lane == lane.id
                && d.region.is_none()
                && d.end > d.start
            {
                shown.push((None, d.start, d.end, ""));
            }
            for (id, start, end, name) in shown {
                for (x0, x1) in self.range_spans(lane_left, start, end) {
                    let area = Rect::from_x_y_ranges(x0..=x1, body.y_range());
                    painter.rect_filled(area, 3.0, REGION.gamma_multiply(0.35));
                    let stroke = if id.is_some() && id == self.selected_range {
                        Stroke::new(2.0, Color32::WHITE)
                    } else {
                        Stroke::new(1.0, REGION)
                    };
                    painter.rect_stroke(area, 3.0, stroke, egui::StrokeKind::Inside);
                    painter.with_clip_rect(area.intersect(painter.clip_rect())).text(
                        pos2(x0.max(lane_left) + 6.0, area.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                }
            }
        }
    }
}
