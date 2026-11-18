//! The editor window: its state, the toolbar, keyboard shortcuts and the window title.

mod clips;
mod effect_menu;
mod export;
mod files;
mod generate;
mod lanes;
mod levels;
mod mixing;
mod mouse;
mod playback;
mod recovery;
mod regions;
mod timeline;
#[cfg(test)]
mod ui_tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use craft_audio::peaks::Peaks;
use egui::{Key, Ui, ViewportCommand};
use mouse::{Drag, Menu};
use playback::Player;
use recovery::Recovery;
use refrain_engine::{ClipId, Command, Frames, Project, Session, Source, TrackId};
use timeline::SCROLLBAR_H;

const HEADER_W: f32 = 130.0;
const TRACK_H: f32 = 110.0;
const HANDLE: f32 = 6.0;
/// The working rates offered for a project and for an export.
const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
const AUDIO_FILES: &[&str] = &["wav", "flac", "mp3", "ogg", "opus", "aif", "aiff", "m4a"];
const HINTS: &str = "Space: play · middle-drag or scrollbar: move along · Ctrl+wheel: zoom · S: split · Ctrl+D: duplicate · F: fit all · \
    drag a clip over another: crossfade · double-click: volume point · right-click point: remove · drag top corners: fades · drag clip edges: trim · right-click: fade curves, channels";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 600.0]),
        ..Default::default()
    };
    let mut editor = Editor {
        recovery: dirs::data_local_dir().map(|dir| Recovery::new(dir.join("refrain"))),
        ..Editor::default()
    };
    editor.offer_recovery();
    eframe::run_native("Refrain", options, Box::new(|_| Ok(Box::new(editor))))
}

struct Editor {
    session: Session,
    player: Player,
    selected: Option<ClipId>,
    status: String,
    /// The status is an error message and is shown in red.
    status_error: bool,
    frames_per_px: f64,
    scroll: f64,
    drag: Option<Drag>,
    /// Where the loop end flag is while it is dragged; the project only changes on release.
    loop_drag: Option<Frames>,
    /// Whether dragged edges snap to clip edges and markers.
    snap: bool,
    /// The snap target a dragged edge sits on, shown as a guide line.
    snap_guide: Option<Frames>,
    /// What the open right-click menu is about.
    menu: Option<Menu>,
    /// Track heights the user dragged; other tracks have the default. A view setting, not part of the project.
    track_heights: HashMap<TrackId, f32>,
    /// The effect range being drawn, moved or resized.
    range_drag: Option<lanes::RangeDrag>,
    /// The selected effect range or region.
    selected_range: Option<refrain_engine::RangeId>,
    region_drag: Option<regions::RegionDrag>,
    region_menu: Option<regions::RegionMenu>,
    /// The name being typed in a region's menu.
    region_name: Option<(refrain_engine::RangeId, String)>,
    /// Tracks whose effect lanes are hidden. A view setting.
    folded: std::collections::HashSet<TrackId>,
    /// An effect setting being dragged in its menu, not yet in the project.
    effect_edit: Option<(
        refrain_engine::LaneId,
        Option<refrain_engine::RangeId>,
        refrain_engine::Effect,
    )>,
    /// A range's fade-in and fade-out being dragged in its menu, in frames, not yet in the project.
    fade_edit: Option<(refrain_engine::RangeId, Frames, Frames)>,
    /// The width of the track lanes in the last frame, for zooming to fit.
    lane_w: f32,
    /// The track whose height is being dragged.
    resizing: Option<TrackId>,
    /// A track volume being dragged, not yet in the project.
    gain_edit: Option<(TrackId, f32)>,
    /// The track whose name is being typed, and the text so far.
    name_edit: Option<(TrackId, String)>,
    /// Waveform overviews, keyed by source address and channel. The weak reference keeps the address
    /// from being reused by new audio while the entry exists.
    peaks: HashMap<(usize, usize), Wave>,
    /// The project file, once opened or saved.
    file: Option<PathBuf>,
    /// The project as last opened or saved; any other snapshot means unsaved changes.
    saved: Arc<Project>,
    saved_heights: Vec<f32>,
    export_open: bool,
    level_row: levels::LevelRow,
    mixer: mixing::Mixer,
    /// The settings panel of a generated clip, when open.
    generate: Option<generate::Panel>,
    /// What "Add generator" makes: the settings last set.
    last_generator: refrain_engine::Generator,
    /// Where unsaved work is copied in case the editor does not close normally. None in tests.
    recovery: Option<Recovery>,
    title: String,
    /// Set after the user agreed to close, so the close request is not asked about again.
    closing: bool,
}

/// A waveform overview of one channel, or of the mix of left and right.
struct Wave {
    source: Weak<Source>,
    peaks: Peaks,
    /// The mixed samples; empty for a channel of the source, which holds its own samples.
    mixed: Vec<f32>,
}

impl Default for Editor {
    fn default() -> Self {
        let session = Session::default();
        Editor {
            saved: session.project().clone(),
            saved_heights: vec![],
            export_open: false,
            generate: None,
            level_row: levels::LevelRow::default(),
            mixer: mixing::Mixer::default(),
            last_generator: refrain_engine::Generator::Tone {
                wave: refrain_engine::Wave::Sine,
                hz: 440.0,
                level_db: -12.0,
            },
            recovery: None,
            file: None,
            title: String::new(),
            closing: false,
            session,
            player: Player::default(),
            selected: None,
            status: "Import audio files to start.".into(),
            status_error: false,
            frames_per_px: 400.0,
            scroll: 0.0,
            drag: None,
            loop_drag: None,
            menu: None,
            snap: true,
            snap_guide: None,
            track_heights: HashMap::new(),
            resizing: None,
            lane_w: 800.0,
            range_drag: None,
            selected_range: None,
            region_drag: None,
            region_menu: None,
            region_name: None,
            folded: std::collections::HashSet::new(),
            effect_edit: None,
            fade_edit: None,
            gain_edit: None,
            name_edit: None,
            peaks: HashMap::new(),
        }
    }
}

impl eframe::App for Editor {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    /// Only reached once the user agreed to close, so whatever is unsaved was meant to be dropped.
    fn on_exit(&mut self) {
        if let Some(recovery) = &mut self.recovery {
            recovery.remove();
            recovery.remove_kept_originals();
        }
    }
}

impl Editor {
    fn show(&mut self, ui: &mut Ui) {
        if let Some(clip) = self.selected
            && self.session.project().clip(clip).is_none()
        {
            self.selected = None;
        }
        self.guard_close(ui);
        self.update_title(ui);
        self.shortcuts(ui);
        self.autosave(ui.ctx());
        self.export_panel(ui.ctx());
        self.generate_panel(ui.ctx());
        egui::Panel::top("toolbar").show(ui, |ui| ui.horizontal_wrapped(|ui| self.toolbar(ui)));
        egui::Panel::bottom("status").show(ui, |ui| {
            if self.status.is_empty() {
                ui.label(HINTS);
            } else if self.status_error {
                ui.colored_label(ui.visuals().error_fg_color, &self.status);
            } else {
                ui.label(&self.status);
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.snap_guide = None;
            self.ruler(ui);
            self.region_lanes(ui);
            let level_row = self.level_row_space(ui);
            let max_height = (ui.available_height() - SCROLLBAR_H - ui.spacing().item_spacing.y).max(0.0);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| self.timeline(ui));
            self.scrollbar(ui);
            self.mix(ui.ctx());
            self.draw_level_row(ui, level_row);
        });
        if self.player.is_playing() {
            ui.ctx().request_repaint();
        }
    }
}

impl Editor {
    fn run(&mut self, command: Command) -> bool {
        match self.session.execute(command) {
            Ok(()) => {
                self.info(String::new());
                true
            }
            Err(e) => {
                self.fail(e.to_string());
                false
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.menu_button("File", |ui| {
            if ui.add(egui::Button::new("New").shortcut_text("Ctrl+N")).clicked() {
                self.new_project();
            }
            if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                self.open();
            }
            if ui.add(egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                self.save();
            }
            if ui
                .add(egui::Button::new("Save as…").shortcut_text("Ctrl+Shift+S"))
                .clicked()
            {
                self.save_as();
            }
            ui.separator();
            if ui
                .button("Export…")
                .on_hover_text("Format, quality and target file")
                .clicked()
            {
                self.export_open = true;
            }
        });
        if ui.button("Import…").clicked() {
            self.import();
        }
        if ui
            .button("Add generator")
            .on_hover_text("A tone, noise or blips as a new clip, on a new track at the play position")
            .clicked()
        {
            self.add_generator();
        }
        if ui.button("Export").on_hover_text(self.export_tooltip()).clicked() {
            self.export_again();
        }
        self.rate_choice(ui);
        ui.separator();
        let label = if self.player.is_playing() { "Stop" } else { "Play" };
        if ui.button(label).clicked() {
            self.toggle_play();
        }
        ui.label(self.time_text(self.player.position()));
        if ui
            .button("Fit")
            .on_hover_text("Zoom to show the whole project. F")
            .clicked()
        {
            self.zoom_to_fit();
        }
        if ui
            .selectable_label(self.snap, "Snap")
            .on_hover_text("Dragged clips and edges snap to other clips' edges, 0, the loop end and the play position. Hold Shift while dragging to place freely.")
            .clicked()
        {
            self.snap = !self.snap;
        }
        let looping = self.session.project().loop_end.is_some();
        if ui
            .selectable_label(looping, "Loop")
            .on_hover_text("Play and export the project as a loop: audio past the loop end fades into the start")
            .clicked()
        {
            let end = self.session.project().clips_end();
            self.run(Command::SetLoopEnd {
                at: if looping { None } else { Some(end) },
            });
        }
        if ui
            .add_enabled(looping, egui::Button::new("Play seam"))
            .on_hover_text("Play from 2 seconds before the loop end, through the join")
            .clicked()
        {
            self.play_seam();
        }
        ui.separator();
        if ui
            .add_enabled(self.session.can_undo(), egui::Button::new("Undo"))
            .clicked()
        {
            self.undo();
        }
        if ui
            .add_enabled(self.session.can_redo(), egui::Button::new("Redo"))
            .clicked()
        {
            self.redo();
        }
    }

    fn shortcuts(&mut self, ui: &mut Ui) {
        let typing = ui.ctx().egui_wants_keyboard_input();
        let new = ui.input(|i| i.modifiers.command && i.key_pressed(Key::N));
        if new {
            self.new_project();
        }
        let (undo, redo, delete, save, save_as, open, export, duplicate, fit) = ui.input(|i| {
            let (cmd, shift) = (i.modifiers.command, i.modifiers.shift);
            (
                cmd && !shift && i.key_pressed(Key::Z),
                cmd && (i.key_pressed(Key::Y) || (shift && i.key_pressed(Key::Z))),
                i.key_pressed(Key::Delete),
                cmd && !shift && i.key_pressed(Key::S),
                cmd && shift && i.key_pressed(Key::S),
                cmd && i.key_pressed(Key::O),
                cmd && i.key_pressed(Key::E),
                cmd && i.key_pressed(Key::D),
                !typing && !cmd && i.key_pressed(Key::F),
            )
        });
        if duplicate {
            self.duplicate();
        }
        if fit {
            self.zoom_to_fit();
        }
        if export {
            self.export_again();
        }
        if save {
            self.save();
        }
        if save_as {
            self.save_as();
        }
        if open {
            self.open();
        }
        // Consumed so a focused button does not also react to Space.
        let space = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Space));
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if delete && let Some(range) = self.selected_range.take() {
            if self.session.project().region(range).is_some() {
                self.run(Command::RemoveRegion { region: range });
            } else {
                self.run(Command::RemoveEffectRange { range });
            }
        } else if delete && let Some(clip) = self.selected {
            self.run(Command::DeleteClip { clip });
        }
        if space {
            self.toggle_play();
        }
    }

    /// Right after the original on its track; where the rules do not allow that, on a new track at the same time.
    fn duplicate(&mut self) {
        let Some(id) = self.selected else {
            self.info("Select a clip to duplicate it.".into());
            return;
        };
        let Some((track, clip)) = self.session.project().clip(id) else {
            return;
        };
        let (track, start, end) = (track.id, clip.start, clip.end());
        let after = Command::DuplicateClip {
            clip: id,
            track: Some(track),
            start: end,
        };
        if self.session.execute(after).is_ok() {
            self.info(String::new());
        } else if self.run(Command::DuplicateClip {
            clip: id,
            track: None,
            start,
        }) {
            self.info("There is no room after the clip, so the copy is on a new track.".into());
        } else {
            return;
        }
        // The copy has the newest id.
        let clips = self.session.project().tracks.iter().flat_map(|t| &t.clips);
        self.selected = clips.map(|c| c.id).max_by_key(|c| c.0);
    }

    fn zoom_to_fit(&mut self) {
        let len = self.session.project().len().max(self.session.project().clips_end());
        if len <= 0 {
            return;
        }
        // A little room after the end, so the last clip's edge can still be grabbed.
        let room = (self.lane_w - 24.0).max(50.0);
        self.frames_per_px = (len as f64 / f64::from(room)).clamp(1.0, 200_000.0);
        self.scroll = 0.0;
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.stop();
        } else if let Err(e) = self.player.play(self.session.project()) {
            self.fail(e);
        }
    }

    /// The project's working rate. Files at other rates are converted to it on import.
    fn rate_choice(&mut self, ui: &mut Ui) {
        let project = self.session.project();
        let current = project.sample_rate;
        let mut chosen = None;
        ui.add_enabled_ui(!project.is_empty(), |ui| {
            egui::ComboBox::from_id_salt("sample rate")
                .selected_text(if current == 0 { "– kHz".into() } else { khz(current) })
                .show_ui(ui, |ui| {
                    for rate in RATES {
                        if ui.selectable_label(rate == current, khz(rate)).clicked() {
                            chosen = Some(rate);
                        }
                    }
                })
                .response
                .on_hover_text(
                    "The project's sample rate. Audio files at other rates are converted to it; the files stay unchanged.",
                )
                .on_disabled_hover_text("Set by the first audio file");
        });
        if let Some(rate) = chosen
            && rate != current
            && self.run(Command::SetSampleRate { rate })
        {
            self.follow_rate(current);
            self.info(format!("The project now runs at {}.", khz(rate)));
        }
    }

    /// The view and the play position are in samples too: after a rate change they keep the same seconds.
    fn follow_rate(&mut self, before: u32) {
        let now = self.session.project().sample_rate;
        if before == 0 || now == 0 || before == now {
            return;
        }
        let ratio = f64::from(now) / f64::from(before);
        self.frames_per_px *= ratio;
        self.scroll *= ratio;
        self.player
            .seek((self.player.position() as f64 * ratio).round() as Frames);
    }

    fn play_seam(&mut self) {
        let project = self.session.project();
        let Some(end) = project.loop_end else { return };
        self.player
            .seek(end.saturating_sub(2 * Frames::from(project.sample_rate)).max(0));
        if !self.player.is_playing() {
            self.toggle_play();
        }
    }

    fn time_text(&self, frames: Frames) -> String {
        let rate = self.session.project().sample_rate;
        let secs = if rate == 0 {
            0.0
        } else {
            frames as f64 / f64::from(rate)
        };
        format!("{}:{:06.3}", (secs / 60.0).floor(), secs % 60.0)
    }

    fn info(&mut self, text: String) {
        self.status = text;
        self.status_error = false;
    }

    fn fail(&mut self, text: String) {
        self.status = text;
        self.status_error = true;
    }

    fn undo(&mut self) {
        let rate = self.session.project().sample_rate;
        if let Err(e) = self.session.undo() {
            self.fail(e.to_string());
        }
        self.follow_rate(rate);
    }

    fn redo(&mut self) {
        let rate = self.session.project().sample_rate;
        if let Err(e) = self.session.redo() {
            self.fail(e.to_string());
        }
        self.follow_rate(rate);
    }

    /// The window shows the file name and a dot while there are unsaved changes.
    fn update_title(&mut self, ui: &Ui) {
        let name = self
            .file
            .as_deref()
            .and_then(|f| f.file_stem())
            .map_or_else(|| "Untitled".into(), |n| n.to_string_lossy().into_owned());
        let dot = if self.has_unsaved_changes() { "• " } else { "" };
        let title = format!("{dot}{name} – Refrain");
        if title != self.title {
            ui.ctx().send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn guard_close(&mut self, ui: &Ui) {
        if ui.input(|i| i.viewport().close_requested()) && !self.closing && self.has_unsaved_changes() {
            ui.ctx().send_viewport_cmd(ViewportCommand::CancelClose);
            if self.may_discard_changes() {
                self.closing = true;
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }
}

fn khz(rate: u32) -> String {
    format!("{} kHz", f64::from(rate) / 1000.0)
}
