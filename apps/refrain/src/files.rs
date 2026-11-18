//! Opening, saving, importing and exporting, and asking before unsaved changes are lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refrain_engine::{Command, PROJECT_EXTENSION, load_source, open_project};
use rfd::{MessageButtons, MessageDialogResult};

use crate::timeline::{MAX_TRACK_H, MIN_TRACK_H};
use crate::{AUDIO_FILES, Editor};

/// The window's settings saved with a project, so it opens looking as it was left.
#[derive(serde::Serialize, serde::Deserialize)]
struct View {
    frames_per_px: f64,
    scroll: f64,
    /// In track order.
    track_heights: Vec<f32>,
    #[serde(default)]
    level_row_height: Option<f32>,
}

impl Editor {
    /// Track heights count as a change because people set them on purpose; zoom and scroll do not.
    pub(crate) fn has_unsaved_changes(&self) -> bool {
        !Arc::ptr_eq(&self.saved, self.session.project()) || self.heights() != self.saved_heights
    }

    /// In track order.
    fn heights(&self) -> Vec<f32> {
        let tracks = self.session.project().tracks.iter();
        tracks
            .map(|t| self.track_heights.get(&t.id).copied().unwrap_or(crate::TRACK_H))
            .collect()
    }

    pub(crate) fn open(&mut self) {
        if !self.may_discard_changes() {
            return;
        }
        let dialog = rfd::FileDialog::new().add_filter("Project", &[PROJECT_EXTENSION]);
        if let Some(path) = dialog.pick_file() {
            self.open_from(&path);
        }
    }

    pub(crate) fn open_from(&mut self, path: &Path) -> bool {
        match open_project(path) {
            Ok((session, view)) => {
                self.start_over(session, Some(path.to_path_buf()));
                self.apply_view(view);
                self.saved_heights = self.heights();
                self.info(format!("Opened {}", path.display()));
                true
            }
            Err(e) => {
                self.fail(format!("{}: {e}", path.display()));
                false
            }
        }
    }

    /// File > New: an empty project, after asking about unsaved changes.
    pub(crate) fn new_project(&mut self) {
        if !self.may_discard_changes() {
            return;
        }
        self.start_over(refrain_engine::Session::default(), None);
        self.info("New project.".into());
    }

    /// Everything the window holds about the project goes with it: ids start again in every project, so a
    /// selection or open menu left over could point at something else in the next.
    fn start_over(&mut self, session: refrain_engine::Session, file: Option<PathBuf>) {
        self.player.stop();
        self.player.seek(0);
        self.session = session;
        self.saved = self.session.project().clone();
        self.file = file;
        self.selected = None;
        self.selected_range = None;
        self.drag = None;
        self.loop_drag = None;
        self.range_drag = None;
        self.region_drag = None;
        self.menu = None;
        self.region_menu = None;
        self.region_name = None;
        self.effect_edit = None;
        self.speed_edit = None;
        self.fade_edit = None;
        self.gain_edit = None;
        self.name_edit = None;
        self.resizing = None;
        self.generate = None;
        self.folded.clear();
        self.track_heights.clear();
        self.saved_heights = vec![];
        self.scroll = 0.0;
        self.peaks.retain(|_, wave| wave.source.strong_count() > 0);
    }

    /// Returns whether the project is saved afterwards.
    pub(crate) fn save(&mut self) -> bool {
        match self.file.clone() {
            Some(path) => self.save_to(&path),
            None => self.save_as(),
        }
    }

    pub(crate) fn save_as(&mut self) -> bool {
        let dialog = rfd::FileDialog::new()
            .add_filter("Project", &[PROJECT_EXTENSION])
            .set_file_name(format!("project.{PROJECT_EXTENSION}"));
        let Some(mut path) = dialog.save_file() else {
            return false;
        };
        if path.extension().is_none() {
            path.set_extension(PROJECT_EXTENSION);
        }
        self.save_to(&path)
    }

    pub(crate) fn save_to(&mut self, path: &Path) -> bool {
        let view = self.view();
        let kept_dir = self.originals_dir();
        match self.session.save(path, Some(&view), kept_dir.as_deref()) {
            Ok(()) => {
                self.saved = self.session.project().clone();
                self.saved_heights = self.heights();
                self.file = Some(path.to_path_buf());
                self.info(format!("Saved {}", path.display()));
                true
            }
            Err(e) => {
                self.fail(format!("Not saved: {e}"));
                false
            }
        }
    }

    pub(crate) fn view(&self) -> serde_json::Value {
        let view = View {
            frames_per_px: self.frames_per_px,
            scroll: self.scroll,
            track_heights: self.heights(),
            level_row_height: Some(self.level_row.height),
        };
        serde_json::to_value(view).unwrap_or_default()
    }

    /// A view from a file is checked like any other input; whatever does not fit is left as it is.
    fn apply_view(&mut self, view: Option<serde_json::Value>) {
        let Some(view) = view.and_then(|v| serde_json::from_value::<View>(v).ok()) else {
            return;
        };
        if view.frames_per_px.is_finite() {
            self.frames_per_px = view.frames_per_px.clamp(1.0, 200_000.0);
        }
        if view.scroll.is_finite() {
            self.scroll = view.scroll.max(0.0);
        }
        if let Some(height) = view.level_row_height.filter(|h| h.is_finite()) {
            self.level_row.height = height.clamp(crate::levels::MIN_ROW_H, crate::levels::MAX_ROW_H);
        }
        let tracks = self.session.project().tracks.iter();
        for (track, height) in tracks.zip(view.track_heights) {
            if height.is_finite() {
                self.track_heights
                    .insert(track.id, height.clamp(MIN_TRACK_H, MAX_TRACK_H));
            }
        }
    }

    /// Asks before unsaved changes are lost. Returns whether it is fine to go on.
    pub(crate) fn may_discard_changes(&mut self) -> bool {
        if !self.has_unsaved_changes() {
            return true;
        }
        let buttons = MessageButtons::YesNoCancelCustom("Save".into(), "Discard".into(), "Cancel".into());
        let answer = rfd::MessageDialog::new()
            .set_title("Unsaved changes")
            .set_description("The project has changes that are not saved.")
            .set_buttons(buttons)
            .show();
        // Some platforms answer with the button label, others with Yes/No.
        match answer {
            MessageDialogResult::Yes => self.save(),
            MessageDialogResult::Custom(label) if label == "Save" => self.save(),
            MessageDialogResult::No => true,
            MessageDialogResult::Custom(label) if label == "Discard" => true,
            _ => false,
        }
    }

    pub(crate) fn import(&mut self) {
        let Some(paths) = rfd::FileDialog::new().add_filter("Audio", AUDIO_FILES).pick_files() else {
            return;
        };
        self.import_files(&paths, 0);
    }

    /// Each file goes onto a new track at `start`, all in one undo step. Files that cannot be read are named
    /// in the status line; the others are still added.
    pub(crate) fn import_files(&mut self, paths: &[PathBuf], start: refrain_engine::Frames) {
        let mut sources = vec![];
        let mut failed = vec![];
        for path in paths {
            match load_source(path) {
                Ok(source) => sources.push(Arc::new(source)),
                Err(e) => failed.push(format!("{}: {e}", path.display())),
            }
        }
        if !sources.is_empty() {
            self.run(Command::AddClipsOnNewTracks { sources, start });
        }
        if !failed.is_empty() {
            self.fail(failed.join(" · "));
        }
    }

    /// Files dropped on the window from outside: audio goes onto new tracks at the play position, a project
    /// opens. The window is not told where they were dropped, so they cannot go under the mouse.
    pub(crate) fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let (hovering, dropped) = ctx.input(|i| (!i.raw.hovered_files.is_empty(), i.raw.dropped_files.clone()));
        if hovering {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop audio to add it on new tracks at the play position, or a project to open it",
                egui::FontId::proportional(18.0),
                egui::Color32::WHITE,
            );
        }
        let paths: Vec<PathBuf> = dropped.iter().map(|f| f.path().to_path_buf()).collect();
        let is_project = |p: &PathBuf| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION));
        let (projects, audio): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(is_project);
        match projects.as_slice() {
            [] if audio.is_empty() => {}
            [] => {
                let start = self.player.position();
                self.import_files(&audio, start);
            }
            [project] if audio.is_empty() => {
                if self.may_discard_changes() {
                    self.open_from(project);
                }
            }
            _ => self.fail("Drop one project on its own to open it, or only audio files to add them".into()),
        }
    }
}
