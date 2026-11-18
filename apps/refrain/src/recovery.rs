//! Recovery copies of unsaved work, so a crash or power cut loses at most the last half minute, and the kept
//! originals of audio files an export overwrote.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use refrain_engine::{PROJECT_EXTENSION, Project, Session, save_project};
use rfd::{MessageButtons, MessageDialogResult};

use crate::Editor;

const EVERY: Duration = Duration::from_secs(30);
const PREFIX: &str = "recovery-";
const ORIGINALS: &str = "originals";

pub(crate) struct Recovery {
    dir: PathBuf,
    /// Starts the names of this window's files.
    tag: String,
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
            tag: name.to_string(),
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

    pub(crate) fn originals_dir(&self) -> PathBuf {
        self.dir.join(ORIGINALS)
    }

    /// Unique, and ends with the original name so it can be given back when the copy moves next to a project.
    pub(crate) fn kept_path(&self, name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        self.originals_dir().join(format!("{}-{nanos}-{name}", self.tag))
    }

    /// At a normal close nothing of this window needs its kept originals any more.
    pub(crate) fn remove_kept_originals(&self) {
        let Ok(entries) = std::fs::read_dir(self.originals_dir()) else {
            return;
        };
        let mine = format!("{}-", self.tag);
        for path in entries.flatten().map(|e| e.path()) {
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&mine)) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// The audio files a project file names, read without loading any audio.
fn files_named_in(project_file: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(project_file) else {
        return vec![];
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let sources = json.get("sources").and_then(|s| s.as_array());
    sources
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str())
        .map(PathBuf::from)
        .collect()
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
        let mut view = self.view();
        if let (Some(view), Some(file)) = (view.as_object_mut(), &self.file) {
            view.insert("file".into(), file.to_string_lossy().into());
        }
        let written = std::fs::create_dir_all(&recovery.dir)
            .map_err(refrain_engine::Error::from)
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
            match self.restore(&copy) {
                Ok(()) => break,
                Err(why) => {
                    rfd::MessageDialog::new()
                        .set_level(rfd::MessageLevel::Error)
                        .set_title("Cannot restore")
                        .set_description(why)
                        .show();
                }
            }
        }
        self.remove_unused_originals();
    }

    /// Kept originals that neither the open project nor any recovery copy uses are left over from a crash.
    pub(crate) fn remove_unused_originals(&self) {
        let Some(recovery) = &self.recovery else {
            return;
        };
        let project = self.session.project();
        let mut used: Vec<PathBuf> = project
            .sources()
            .iter()
            .filter_map(|s| project.file_of(s))
            .map(Path::to_path_buf)
            .collect();
        for (copy, _) in recovery.left_behind() {
            used.extend(files_named_in(&copy));
        }
        let used: Vec<_> = used.iter().filter_map(|f| f.file_name()).collect();
        let Ok(entries) = std::fs::read_dir(recovery.originals_dir()) else {
            return;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.file_name().is_some_and(|n| !used.contains(&n)) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    pub(crate) fn originals_dir(&self) -> Option<PathBuf> {
        self.recovery.as_ref().map(Recovery::originals_dir)
    }

    /// Opens a recovery copy as unsaved work of the project it came from, and deletes the copy. A copy that
    /// cannot be opened is renamed, so it is kept but not offered at every start again.
    pub(crate) fn restore(&mut self, copy: &Path) -> Result<(), String> {
        if !self.open_from(copy) {
            let name = copy
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let aside = copy.with_file_name(format!("unreadable-{name}"));
            let kept = match std::fs::rename(copy, &aside) {
                Ok(()) => format!("The copy is kept as {}.", aside.display()),
                Err(_) => format!("The copy stays at {}.", copy.display()),
            };
            return Err(format!("{} {kept}", self.status));
        }
        self.file = original_file(copy);
        // Nothing of this is saved yet.
        self.saved = Session::default().project().clone();
        let _ = std::fs::remove_file(copy);
        self.info("Restored unsaved work. Save to keep it.".into());
        Ok(())
    }
}
