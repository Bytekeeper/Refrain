mod files;
mod mouse;
mod playback;
mod timeline;
#[cfg(test)]
mod ui_tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use audioeditor_engine::{ClipId, Command, Frames, Project, Session, Source, TrackId};
use craft_audio::peaks::Peaks;
use egui::{Key, Ui, ViewportCommand};
use mouse::{Drag, FadeMenu};
use playback::Player;
use timeline::SCROLLBAR_H;

const HEADER_W: f32 = 110.0;
const TRACK_H: f32 = 110.0;
const HANDLE: f32 = 6.0;
const AUDIO_FILES: &[&str] = &["wav", "flac", "mp3", "ogg", "opus", "aif", "aiff", "m4a"];
const HINTS: &str = "Space: play · middle-drag or scrollbar: move along · Ctrl+wheel: zoom · S: split · \
    drag a clip over another: crossfade · double-click: volume point · right-click point: remove · drag top corners: fades · drag clip edges: trim · right-click a crossfade or fade: curve";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("Audio editor", options, Box::new(|_| Ok(Box::new(Editor::default()))))
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
    /// The fade whose right-click menu is open.
    fade_menu: Option<FadeMenu>,
    /// Track heights the user dragged; other tracks have the default. A view setting, not part of the project.
    track_heights: HashMap<TrackId, f32>,
    /// The track whose height is being dragged.
    resizing: Option<TrackId>,
    /// Waveform overviews, keyed by source address and channel. The weak reference keeps the address
    /// from being reused by new audio while the entry exists.
    peaks: HashMap<(usize, usize), (Weak<Source>, Peaks)>,
    /// The project file, once opened or saved.
    file: Option<PathBuf>,
    /// The project as last opened or saved; any other snapshot means unsaved changes.
    saved: Arc<Project>,
    title: String,
    /// Set after the user agreed to close, so the close request is not asked about again.
    closing: bool,
}

impl Default for Editor {
    fn default() -> Self {
        let session = Session::default();
        Editor {
            saved: session.project().clone(),
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
            fade_menu: None,
            track_heights: HashMap::new(),
            resizing: None,
            peaks: HashMap::new(),
        }
    }
}

impl eframe::App for Editor {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
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
        egui::Panel::top("toolbar").show(ui, |ui| ui.horizontal(|ui| self.toolbar(ui)));
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
            self.ruler(ui);
            let max_height = (ui.available_height() - SCROLLBAR_H - ui.spacing().item_spacing.y).max(0.0);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| self.timeline(ui));
            self.scrollbar(ui);
        });
        self.player.set_project(self.session.project());
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
        if ui.button("Open…").clicked() {
            self.open();
        }
        if ui.button("Save").clicked() {
            self.save();
        }
        if ui.button("Save as…").clicked() {
            self.save_as();
        }
        ui.separator();
        if ui.button("Import…").clicked() {
            self.import();
        }
        if ui.button("Export WAV…").clicked() {
            self.export();
        }
        self.rate_choice(ui);
        ui.separator();
        let label = if self.player.is_playing() { "Stop" } else { "Play" };
        if ui.button(label).clicked() {
            self.toggle_play();
        }
        ui.label(self.time_text(self.player.position()));
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
        ui.separator();
        if let Some(clip) = self.selected
            && ui.button("Delete").clicked()
        {
            self.run(Command::DeleteClip { clip });
        }
    }

    fn shortcuts(&mut self, ui: &mut Ui) {
        let (undo, redo, delete, save, save_as, open) = ui.input(|i| {
            let (cmd, shift) = (i.modifiers.command, i.modifiers.shift);
            (
                cmd && !shift && i.key_pressed(Key::Z),
                cmd && (i.key_pressed(Key::Y) || (shift && i.key_pressed(Key::Z))),
                i.key_pressed(Key::Delete),
                cmd && !shift && i.key_pressed(Key::S),
                cmd && shift && i.key_pressed(Key::S),
                cmd && i.key_pressed(Key::O),
            )
        });
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
        if delete && let Some(clip) = self.selected {
            self.run(Command::DeleteClip { clip });
        }
        if space {
            self.toggle_play();
        }
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
        const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
        let project = self.session.project();
        let current = project.sample_rate;
        let khz = |rate: u32| format!("{} kHz", f64::from(rate) / 1000.0);
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
        let title = format!("{dot}{name} – Audio editor");
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
