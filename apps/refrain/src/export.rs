//! The export panel and "export again": one target per project, written with the same settings every time.

use std::path::Path;

use egui::{Align2, Order, Ui, vec2};
use refrain_engine::{Command, ExportFormat, ExportPart, ExportSettings};

use crate::{Editor, RATES, khz};

const KBPS: [u32; 6] = [96, 128, 160, 192, 256, 320];

/// For the status line and the tooltip, e.g. "Ogg Vorbis, 160 kbit/s, mono, 48 kHz".
fn summary(s: &ExportSettings, rate: u32) -> String {
    let quality = if s.format.is_lossy() {
        format!("{} kbit/s", s.kbps)
    } else {
        depth_name(s.bits)
    };
    let channels = if s.mono { "mono" } else { "stereo" };
    format!("{}, {quality}, {channels}, {}", s.format.name(), khz(rate))
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

/// The rows of a two-column grid that set how audio is written.
pub(crate) fn settings_rows(ui: &mut Ui, s: &mut ExportSettings, project_rate: u32) {
    let old = s.format;
    let formats = ExportFormat::ALL.map(|f| (f, f.name().to_string()));
    choice(ui, "Format", &mut s.format, &formats);
    if s.format.is_lossy() {
        let rates = KBPS.map(|k| (k, format!("{k} kbit/s")));
        choice(ui, "Bit rate", &mut s.kbps, &rates);
    } else {
        let depths: &[u16] = if s.format == ExportFormat::Wav {
            &[16, 24, 32]
        } else {
            &[16, 24]
        };
        let depths: Vec<_> = depths.iter().map(|&b| (b, depth_name(b))).collect();
        choice(ui, "Bit depth", &mut s.bits, &depths);
    }
    ui.label("Channels");
    ui.horizontal(|ui| {
        ui.radio_value(&mut s.mono, false, "Stereo");
        ui.radio_value(&mut s.mono, true, "Mono")
            .on_hover_text("Left and right mixed into one channel, as game engines want for sounds placed in space");
    });
    ui.end_row();
    let mut rates = vec![(None, format!("Project ({})", khz(project_rate)))];
    rates.extend(RATES.map(|r| (Some(r), khz(r))));
    choice(ui, "Sample rate", &mut s.sample_rate, &rates);
    if s.format != old {
        fit_bits(s);
    }
}

/// A grid row with a list to pick from. A submenu, not a combo box: egui keeps one popup open at a time, so a
/// combo box would close the menu these settings also appear in.
fn choice<T: PartialEq + Copy>(ui: &mut Ui, label: &str, value: &mut T, options: &[(T, String)]) {
    let label = ui.label(label);
    let shown = options
        .iter()
        .find(|(v, _)| v == value)
        .map_or("", |(_, name)| name.as_str());
    // Room on the right for the arrow, which no bundled font has, so it is drawn.
    let response = ui
        .menu_button(format!("{shown}     "), |ui| {
            for (option, name) in options {
                // No `ui.close()`: it would close the menu around these settings too. The list closes as soon
                // as the pointer moves on.
                if ui.selectable_label(option == value, name).clicked() {
                    *value = *option;
                }
            }
        })
        .response
        .labelled_by(label.id);
    let tip = egui::pos2(response.rect.right() - 10.0, response.rect.center().y + 2.5);
    let color = ui.style().interact(&response).fg_stroke.color;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![tip, tip + vec2(-4.0, -5.0), tip + vec2(4.0, -5.0)],
        color,
        egui::Stroke::NONE,
    ));
    ui.end_row();
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
        let old = project.export.clone();
        let mut target = old.clone();
        let mut browse = false;
        egui::Grid::new("export settings")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                settings_rows(ui, &mut target.settings, project_rate);
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

        if target.settings.format != old.settings.format
            && let Some(file) = &mut target.file
        {
            file.set_extension(target.settings.format.extension());
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
        if target != old {
            self.run(Command::SetExportTarget { target });
        }
        if browse {
            self.browse_export_file();
        }
        ui.add_space(4.0);
        let parts = self.export_parts();
        let ranges = self.ranges_text();
        if !ranges.is_empty() {
            ui.weak(format!("Export also writes {ranges} of the export lanes."));
        }
        if ui
            .add_enabled(can_export && !parts.is_empty(), egui::Button::new("Export"))
            .on_hover_text(match (self.session.project().export.file.is_some(), ranges) {
                (true, r) if r.is_empty() => "The mix. Ctrl+E exports again, also when this panel is closed".into(),
                (true, r) => format!("The mix and {r} of the export lanes. Ctrl+E exports them again"),
                (false, r) => format!("{r} of the export lanes; choose a file to export the mix too"),
            })
            .clicked()
        {
            self.write_export(&parts);
        }
    }
    fn browse_export_file(&mut self) {
        let mut target = self.session.project().export.clone();
        let format = target.settings.format;
        let current = target.file.clone();
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
                target.settings.format = format;
                fit_bits(&mut target.settings);
            }
            None => {
                path.set_extension(format.extension());
            }
        }
        target.file = Some(path);
        self.run(Command::SetExportTarget { target });
    }

    /// What "export again" writes: the mix when it has a file, and every region whose lane has a folder.
    fn export_parts(&self) -> Vec<ExportPart> {
        let project = self.session.project();
        let mix = project.export.file.is_some().then_some(ExportPart::Mix);
        let lanes = project.region_lanes.iter().filter(|l| l.folder.is_some());
        let regions = lanes.flat_map(|l| &l.regions).map(|r| ExportPart::Region(r.id));
        mix.into_iter().chain(regions).collect()
    }

    /// The toolbar button and Ctrl+E: writes the mix and the ranges again, or opens the panel while the mix has
    /// no file, so the panel can always be reached.
    pub(crate) fn export_again(&mut self) {
        if self.session.project().export.file.is_some() {
            self.write_export(&self.export_parts());
        } else {
            self.export_open = true;
        }
    }

    /// "and 3 ranges" for the ranges written with the mix; empty without any.
    fn ranges_text(&self) -> String {
        let project = self.session.project();
        match self.export_parts().len() - usize::from(project.export.file.is_some()) {
            0 => String::new(),
            1 => "1 range".into(),
            n => format!("{n} ranges"),
        }
    }

    pub(crate) fn export_tooltip(&self) -> String {
        let project = self.session.project();
        let target = &project.export;
        let Some(file) = &target.file else {
            return "Choose where and how to export. Ctrl+E".into();
        };
        let wanted = target.settings.sample_rate.unwrap_or(project.sample_rate);
        let rate = target.settings.format.rate_for(wanted);
        let ranges = self.ranges_text();
        let and = if ranges.is_empty() {
            ranges
        } else {
            format!(", and {ranges}")
        };
        format!(
            "Export to {} ({}){and}. Ctrl+E",
            file.display(),
            summary(&target.settings, rate)
        )
    }

    /// Writes `parts`. Audio the project uses is kept in this window's originals folder first, so the clips keep
    /// their audio and every later export starts from the original again.
    pub(crate) fn write_export(&mut self, parts: &[ExportPart]) {
        let recovery = self.recovery.as_ref();
        let keep_as = |name: &str| recovery.map(|r| r.kept_path(name));
        let done = match self.session.export(parts, keep_as) {
            Ok(done) => done,
            Err(e) => {
                self.fail(format!("Not exported: {e}"));
                return;
            }
        };
        let text = match done.as_slice() {
            [one] => {
                let settings = self.settings_of(parts.first());
                format!(
                    "Exported {} ({}).",
                    one.file.display(),
                    summary(&settings, one.written.rate)
                )
            }
            many => format!("Exported {} files.", many.len()),
        };
        let kept = if done.iter().any(|d| d.kept_original) {
            " The project keeps using the original audio."
        } else {
            ""
        };
        let over: Vec<(&refrain_engine::Exported, &ExportPart)> =
            done.iter().zip(parts).filter(|(d, _)| d.written.over > 0).collect();
        if over.is_empty() {
            self.info(format!("{text}{kept}"));
            return;
        }
        let float = |p: &ExportPart| {
            let s = self.settings_of(Some(p));
            s.format == ExportFormat::Wav && s.bits == 32
        };
        let fate = if over.iter().all(|(_, p)| float(p)) {
            "stay over in the file, and most game engines cut them off"
        } else {
            "were cut off"
        };
        let loudest: Vec<String> = over
            .iter()
            .map(|(d, _)| {
                let name = d
                    .file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let db = 20.0 * d.written.peak.log10();
                format!("{name} {db:+.1} dB at {}", self.time_text(d.written.peak_at))
            })
            .collect();
        self.fail(format!(
            "{text}{kept} But samples went over 0 dB and {fate}; the loudest: {}. \
             Lower a volume, or the target of a normalize. The Mix row shows where.",
            loudest.join(", ")
        ));
    }

    /// The settings a part is written with.
    fn settings_of(&self, part: Option<&ExportPart>) -> ExportSettings {
        let project = self.session.project();
        match part {
            Some(ExportPart::Region(id)) => project.region(*id).map(|(l, _)| l.settings.clone()),
            _ => None,
        }
        .unwrap_or_else(|| project.export.settings.clone())
    }
}
