//! The export panel and "export again": one target per project, written with the same settings every time.

use std::path::{Path, PathBuf};

use audioeditor_engine::{ExportFormat, ExportSettings, export};
use egui::{Align2, Order, Ui, vec2};

use crate::{Editor, RATES, khz};

const KBPS: [u32; 6] = [96, 128, 160, 192, 256, 320];

/// Where and how the mix is exported. Saved with the project.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExportTarget {
    pub(crate) file: Option<PathBuf>,
    pub(crate) settings: ExportSettings,
}

impl ExportTarget {
    /// For the status line and the tooltip, e.g. "Ogg Vorbis, 160 kbit/s, mono, 48 kHz".
    fn summary(&self, rate: u32) -> String {
        let s = &self.settings;
        let quality = if s.format.is_lossy() {
            format!("{} kbit/s", s.kbps)
        } else {
            depth_name(s.bits)
        };
        let channels = if s.mono { "mono" } else { "stereo" };
        format!("{}, {quality}, {channels}, {}", s.format.name(), khz(rate))
    }
}

fn depth_name(bits: u16) -> String {
    if bits == 32 {
        "32-bit float".into()
    } else {
        format!("{bits}-bit")
    }
}

/// Keeps the bit depth one the format can write.
fn fit_bits(settings: &mut ExportSettings) {
    let allowed: &[u16] = match settings.format {
        ExportFormat::Wav => &[16, 24, 32],
        _ => &[16, 24],
    };
    if !allowed.contains(&settings.bits) {
        settings.bits = 24;
    }
}

impl Editor {
    pub(crate) fn export_panel(&mut self, ctx: &egui::Context) {
        if !self.export_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Export")
            .open(&mut open)
            .order(Order::Foreground)
            .pivot(Align2::RIGHT_TOP)
            .default_pos(ctx.content_rect().right_top() + vec2(-16.0, 70.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| self.export_ui(ui));
        self.export_open &= open;
    }

    fn export_ui(&mut self, ui: &mut Ui) {
        let project = self.session.project();
        let project_rate = project.sample_rate;
        let looping = project.loop_end.is_some();
        let can_export = !project.is_empty();
        let mut target = self.export_target.clone();
        let mut browse = false;
        let s = &mut target.settings;
        egui::Grid::new("export settings")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                let label = ui.label("Format");
                egui::ComboBox::from_id_salt("export format")
                    .selected_text(s.format.name())
                    .show_ui(ui, |ui| {
                        for format in ExportFormat::ALL {
                            ui.selectable_value(&mut s.format, format, format.name());
                        }
                    })
                    .response
                    .labelled_by(label.id);
                ui.end_row();
                if s.format.is_lossy() {
                    let label = ui.label("Bit rate");
                    egui::ComboBox::from_id_salt("export kbps")
                        .selected_text(format!("{} kbit/s", s.kbps))
                        .show_ui(ui, |ui| {
                            for kbps in KBPS {
                                ui.selectable_value(&mut s.kbps, kbps, format!("{kbps} kbit/s"));
                            }
                        })
                        .response
                        .labelled_by(label.id);
                } else {
                    let label = ui.label("Bit depth");
                    let depths: &[u16] = if s.format == ExportFormat::Wav {
                        &[16, 24, 32]
                    } else {
                        &[16, 24]
                    };
                    egui::ComboBox::from_id_salt("export bits")
                        .selected_text(depth_name(s.bits))
                        .show_ui(ui, |ui| {
                            for &bits in depths {
                                ui.selectable_value(&mut s.bits, bits, depth_name(bits));
                            }
                        })
                        .response
                        .labelled_by(label.id);
                }
                ui.end_row();
                ui.label("Channels");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.mono, false, "Stereo");
                    ui.radio_value(&mut s.mono, true, "Mono").on_hover_text(
                        "Left and right mixed into one channel, as game engines want for sounds placed in space",
                    );
                });
                ui.end_row();
                let label = ui.label("Sample rate");
                let project_choice = format!("Project ({})", khz(project_rate));
                egui::ComboBox::from_id_salt("export rate")
                    .selected_text(s.sample_rate.map_or(project_choice.clone(), khz))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.sample_rate, None, project_choice);
                        for rate in RATES {
                            ui.selectable_value(&mut s.sample_rate, Some(rate), khz(rate));
                        }
                    })
                    .response
                    .labelled_by(label.id);
                ui.end_row();
                ui.label("File");
                ui.horizontal(|ui| {
                    let name = target.file.as_deref().and_then(Path::file_name);
                    match name {
                        Some(name) => {
                            let path = target.file.as_deref().map(Path::display).map(|p| p.to_string());
                            ui.label(name.to_string_lossy()).on_hover_text(path.unwrap_or_default());
                        }
                        None => {
                            ui.weak("none chosen");
                        }
                    }
                    browse = ui.button("Browse…").clicked();
                });
                ui.end_row();
            });

        if target.settings.format != self.export_target.settings.format {
            fit_bits(&mut target.settings);
            if let Some(file) = &mut target.file {
                file.set_extension(target.settings.format.extension());
            }
        }
        let format = target.settings.format;
        let wanted = target.settings.sample_rate.unwrap_or(project_rate);
        let written = format.rate_for(wanted);
        if written != wanted {
            ui.label(format!(
                "{} has no {}; the file is written at {}.",
                format.name(),
                khz(wanted),
                khz(written)
            ));
        }
        let warning = ui.visuals().warn_fg_color;
        if looping && format == ExportFormat::Mp3 {
            ui.colored_label(
                warning,
                "MP3 adds silence at the start and end, so the loop gets a gap in most game engines. \
                 Use Ogg Vorbis or WAV for loops.",
            );
        }
        if looping && format == ExportFormat::M4a {
            ui.colored_label(
                warning,
                "M4A only loops without a gap in players that read its length information. \
                 Ogg Vorbis is safer for loops.",
            );
        }
        self.export_target = target;
        if browse {
            self.browse_export_file();
        }
        ui.add_space(4.0);
        let ready = can_export && self.export_target.file.is_some();
        if ui
            .add_enabled(ready, egui::Button::new("Export"))
            .on_hover_text("Ctrl+E exports again with these settings, also when this panel is closed")
            .clicked()
        {
            self.export_again();
        }
    }

    fn browse_export_file(&mut self) {
        let format = self.export_target.settings.format;
        let current = self.export_target.file.clone();
        let name = current
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                let stem = self.file.as_deref().and_then(Path::file_stem);
                let stem = stem.map_or_else(|| "mix".into(), |s| s.to_string_lossy().into_owned());
                format!("{stem}.{}", format.extension())
            });
        let mut dialog = rfd::FileDialog::new()
            .add_filter(format.name(), &[format.extension()])
            .set_file_name(name);
        if let Some(dir) = current.as_deref().or(self.file.as_deref()).and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut path) = dialog.save_file() else {
            return;
        };
        let chosen = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(ExportFormat::from_extension);
        match chosen {
            Some(format) => {
                self.export_target.settings.format = format;
                fit_bits(&mut self.export_target.settings);
            }
            None => {
                path.set_extension(format.extension());
            }
        }
        self.export_target.file = Some(path);
    }

    /// The toolbar button and Ctrl+E: writes the target again, or opens the panel when there is none.
    pub(crate) fn export_again(&mut self) {
        match self.export_target.file.clone() {
            Some(path) => self.write_export(&path),
            None => self.export_open = true,
        }
    }

    pub(crate) fn export_tooltip(&self) -> String {
        match &self.export_target.file {
            Some(file) => {
                let rate = self.export_target.settings.sample_rate;
                let rate = self
                    .export_target
                    .settings
                    .format
                    .rate_for(rate.unwrap_or(self.session.project().sample_rate));
                format!(
                    "Export to {} ({}). Ctrl+E",
                    file.display(),
                    self.export_target.summary(rate)
                )
            }
            None => "Choose where and how to export. Ctrl+E".into(),
        }
    }

    /// When the target is audio the project uses, that file is copied aside first, so the clips keep their
    /// audio and every later export starts from the original again.
    pub(crate) fn write_export(&mut self, path: &Path) {
        let keeps_original = self.session.uses_file(path);
        if keeps_original && let Err(e) = self.keep_original(path) {
            self.fail(format!(
                "Not exported: cannot keep a copy of the original {}: {e}",
                path.display()
            ));
            return;
        }
        match export(self.session.project(), &self.export_target.settings, path) {
            Ok(rate) => {
                let summary = self.export_target.summary(rate);
                let kept = if keeps_original {
                    " The project keeps using the original audio."
                } else {
                    ""
                };
                self.info(format!("Exported {} ({summary}).{kept}", path.display()));
            }
            Err(e) => self.fail(format!("Not exported: {e}")),
        }
    }

    fn keep_original(&mut self, path: &Path) -> std::io::Result<()> {
        let recovery = self.recovery.as_ref();
        let recovery = recovery.ok_or_else(|| std::io::Error::other("there is no folder for it"))?;
        std::fs::create_dir_all(recovery.originals_dir())?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let copy = recovery.kept_path(&name);
        std::fs::copy(path, &copy)?;
        self.session.relocate(path, &copy);
        Ok(())
    }
}
