//! Recovery copies of unsaved work, so a crash or power cut loses at most the last half minute.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use audioeditor_engine::{PROJECT_EXTENSION, Project, Session, save_project};
use rfd::{MessageButtons, MessageDialogResult};

use crate::Editor;

const EVERY: Duration = Duration::from_secs(30);
const PREFIX: &str = "recovery-";

pub(crate) struct Recovery {
    dir: PathBuf,
    /// This window's copy. Copies with other names were left by editors that did not close normally.
    file: PathBuf,
    /// The project last written to `file`, while the file exists.
    written: Option<Arc<Project>>,
    last_write: Option<Instant>,
}

impl Recovery {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self::named(dir, &std::process::id().to_string())
    }

    pub(crate) fn named(dir: PathBuf, name: &str) -> Self {
        let file = dir.join(format!("{PREFIX}{name}.{PROJECT_EXTENSION}"));
        Recovery {
            dir,
            file,
            written: None,
            last_write: None,
        }
    }

    /// Newest first.
    pub(crate) fn left_behind(&self) -> Vec<(PathBuf, SystemTime)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return vec![];
        };
        let mut found: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| *p != self.file && is_recovery(p))
            .filter_map(|p| {
                let time = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                Some((p, time))
            })
            .collect();
        found.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
        found
    }

    pub(crate) fn remove(&mut self) {
        if self.written.take().is_some() {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

fn is_recovery(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    name.starts_with(PREFIX) && path.extension().is_some_and(|e| e == PROJECT_EXTENSION)
}

/// The project file the recovered work belongs to, read without loading any audio.
fn original_file(copy: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(copy).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("view")?.get("file")?.as_str().map(PathBuf::from)
}

impl Editor {
    /// Runs every frame: writes the unsaved project at most every 30 seconds and removes the copy once saved.
    pub(crate) fn autosave(&mut self, ctx: &egui::Context) {
        let Some(mut recovery) = self.recovery.take() else {
            return;
        };
        let project = self.session.project();
        if !self.has_unsaved_changes() {
            recovery.remove();
        } else if !recovery.written.as_ref().is_some_and(|w| Arc::ptr_eq(w, project)) {
            let wait = recovery
                .last_write
                .map_or(Duration::ZERO, |t| EVERY.saturating_sub(t.elapsed()));
            if wait.is_zero() {
                self.write_recovery(&mut recovery);
            } else {
                ctx.request_repaint_after(wait);
            }
        }
        self.recovery = Some(recovery);
    }

    fn write_recovery(&mut self, recovery: &mut Recovery) {
        recovery.last_write = Some(Instant::now());
        let mut view = self.view(None);
        if let (Some(view), Some(file)) = (view.as_object_mut(), &self.file) {
            view.insert("file".into(), file.to_string_lossy().into());
        }
        let written = std::fs::create_dir_all(&recovery.dir)
            .map_err(audioeditor_engine::Error::from)
            .and_then(|()| save_project(self.session.project(), &recovery.file, Some(&view)));
        match written {
            Ok(()) => recovery.written = Some(self.session.project().clone()),
            Err(e) => self.fail(format!("No recovery copy of the unsaved work: {e}")),
        }
    }

    /// Asks about each copy left by an editor that did not close normally, until one is restored.
    pub(crate) fn offer_recovery(&mut self) {
        let Some(recovery) = &self.recovery else {
            return;
        };
        for (copy, time) in recovery.left_behind() {
            let name = original_file(&copy)
                .and_then(|f| f.file_stem().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "Untitled".into());
            let minutes = time.elapsed().map_or(0, |d| d.as_secs() / 60);
            let buttons = MessageButtons::YesNoCancelCustom("Restore".into(), "Delete".into(), "Later".into());
            let answer = rfd::MessageDialog::new()
                .set_title("Unsaved work found")
                .set_description(format!(
                    "The editor closed without saving \"{name}\". The last copy is from {minutes} minutes ago. \
                     Restore it?"
                ))
                .set_buttons(buttons)
                .show();
            match answer {
                MessageDialogResult::Yes => {}
                MessageDialogResult::Custom(label) if label == "Restore" => {}
                MessageDialogResult::No => {
                    let _ = std::fs::remove_file(&copy);
                    continue;
                }
                MessageDialogResult::Custom(label) if label == "Delete" => {
                    let _ = std::fs::remove_file(&copy);
                    continue;
                }
                _ => continue,
            }
            if self.restore(&copy) {
                return;
            }
        }
    }

    /// Opens a recovery copy as unsaved work of the project it came from, and deletes the copy.
    pub(crate) fn restore(&mut self, copy: &Path) -> bool {
        if !self.open_from(copy) {
            return false;
        }
        self.file = original_file(copy);
        // Nothing of this is saved yet.
        self.saved = Session::default().project().clone();
        let _ = std::fs::remove_file(copy);
        self.info("Restored unsaved work. Save to keep it.".into());
        true
    }
}
