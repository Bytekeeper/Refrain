//! Opening, saving, importing and exporting, and asking before unsaved changes are lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refrain_engine::{Command, PROJECT_EXTENSION, load_source, open_project, save_project};
use rfd::{MessageButtons, MessageDialogResult};

use crate::export::ExportTarget;
use crate::timeline::{MAX_TRACK_H, MIN_TRACK_H};
use crate::{AUDIO_FILES, Editor};

/// The window's settings saved with a project, so it opens looking as it was left.
#[derive(serde::Serialize, serde::Deserialize)]
struct View {
    frames_per_px: f64,
    scroll: f64,
    /// In track order.
    track_heights: Vec<f32>,
    /// Its file is relative to the project file when it lies below it.
    #[serde(default)]
    export: ExportTarget,
}

impl Editor {
    /// Track heights count as a change because people set them on purpose; zoom and scroll do not.
    pub(crate) fn has_unsaved_changes(&self) -> bool {
        !Arc::ptr_eq(&self.saved, self.session.project())
            || self.heights() != self.saved_heights
            || self.export_target != self.saved_export
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
                self.player.stop();
                self.player.seek(0);
                self.session = session;
                self.saved = self.session.project().clone();
                self.file = Some(path.to_path_buf());
                self.selected = None;
                self.drag = None;
                self.scroll = 0.0;
                self.track_heights.clear();
                self.export_target = ExportTarget::default();
                self.apply_view(view, path.parent());
                self.saved_heights = self.heights();
                self.saved_export = self.export_target.clone();
                self.peaks.retain(|_, wave| wave.source.strong_count() > 0);
                self.info(format!("Opened {}", path.display()));
                true
            }
            Err(e) => {
                self.fail(format!("{}: {e}", path.display()));
                false
            }
        }
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
        if let Err(e) = self.take_originals_along(path) {
            self.fail(format!(
                "Not saved: cannot copy the kept original audio next to the project: {e}"
            ));
            return false;
        }
        let view = self.view(std::path::absolute(path).ok().as_deref().and_then(Path::parent));
        match save_project(self.session.project(), path, Some(&view)) {
            Ok(()) => {
                self.saved = self.session.project().clone();
                self.saved_heights = self.heights();
                self.saved_export = self.export_target.clone();
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

    /// With `dir`, the export file is stored relative to it where it can be.
    pub(crate) fn view(&self, dir: Option<&Path>) -> serde_json::Value {
        let mut export = self.export_target.clone();
        if let (Some(dir), Some(file)) = (dir, &mut export.file)
            && let Ok(relative) = file.strip_prefix(dir)
        {
            *file = relative.to_path_buf();
        }
        let view = View {
            frames_per_px: self.frames_per_px,
            scroll: self.scroll,
            track_heights: self.heights(),
            export,
        };
        serde_json::to_value(view).unwrap_or_default()
    }

    /// A view from a file is checked like any other input; whatever does not fit is left as it is.
    fn apply_view(&mut self, view: Option<serde_json::Value>, dir: Option<&Path>) {
        let Some(view) = view.and_then(|v| serde_json::from_value::<View>(v).ok()) else {
            return;
        };
        if view.frames_per_px.is_finite() {
            self.frames_per_px = view.frames_per_px.clamp(1.0, 200_000.0);
        }
        if view.scroll.is_finite() {
            self.scroll = view.scroll.max(0.0);
        }
        let tracks = self.session.project().tracks.iter();
        for (track, height) in tracks.zip(view.track_heights) {
            if height.is_finite() {
                self.track_heights
                    .insert(track.id, height.clamp(MIN_TRACK_H, MAX_TRACK_H));
            }
        }
        let mut export = view.export;
        if let (Some(dir), Some(file)) = (dir, &mut export.file)
            && file.is_relative()
        {
            *file = dir.join(&*file);
        }
        self.export_target = export;
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
        self.import_files(&paths);
    }

    /// Each file goes onto a new track at 0.
    pub(crate) fn import_files(&mut self, paths: &[PathBuf]) {
        for path in paths {
            let source = match load_source(path) {
                Ok(source) => Arc::new(source),
                Err(e) => {
                    self.fail(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            if !self.run(Command::AddTrack {
                name: source.name.clone(),
            }) {
                continue;
            }
            let Some(track) = self.session.project().tracks.last().map(|t| t.id) else {
                continue;
            };
            if !self.run(Command::AddClip {
                track,
                source,
                start: 0,
            }) {
                let error = std::mem::take(&mut self.status);
                self.undo();
                self.fail(format!("{}: {error}", path.display()));
            }
        }
    }
}
