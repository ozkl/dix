//! *Convert disk image…*: the whole disk of an open image (its container
//! read through, all partitions) written anew as a raw image or another
//! container (qcow2, VHD, VHDX, VMDK, VDI, DMG), holding only the data.
//! Natively it is written to a file chosen by the user, on a thread, with
//! progress and Cancel; in the browser it is made in memory and downloaded.

use super::{theme, Action};
use crate::model::{VolumeId, Workspace};
use eframe::egui::{self, RichText, Ui};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

pub struct ConvertDialog {
    pub volume: VolumeId,
    /// The container format's name; None: raw.
    pub format: Option<String>,
    pub running: Option<Job>,
    pub error: Option<String>,
    /// The whole disk's size (read when the dialog opened).
    pub disk_size: Option<u64>,
}

pub struct Job {
    pub done: Arc<AtomicU64>,
    pub total: u64,
    pub cancel: Arc<AtomicBool>,
    /// The saved file's name and the bytes of data written, or why not.
    pub result: Receiver<Result<(String, u64), String>>,
}

impl ConvertDialog {
    pub fn new(ws: &Workspace, volume: VolumeId) -> Self {
        let disk_size = ws.whole_disk_size(volume);
        ConvertDialog { volume, format: Some("qcow2".into()), running: None, error: None, disk_size }
    }
}

/// The formats a disk can be converted to: (format name or None for raw,
/// extension, description).
pub fn targets(ws: &Workspace) -> Vec<(Option<String>, &'static str, &'static str)> {
    let mut out = vec![(None, "img", "raw image: the disk's bytes as they are")];
    for f in ws.registry.formats() {
        if let Some((ext, about)) = f.creates() {
            out.push((Some(f.name().to_owned()), ext, about));
        }
    }
    out
}

pub fn show(ui: &mut Ui, ws: &Workspace, d: &mut ConvertDialog, actions: &mut Vec<Action>, close: &mut bool) {
    let Some(v) = ws.volumes.get(&d.volume) else {
        *close = true;
        return;
    };
    ui.heading("Convert disk image");
    ui.add_space(4.0);
    let disk_size = d.disk_size.map(super::format_size).unwrap_or_default();
    let what = match &v.source.container {
        Some(c) => format!("{} ({c}) · {disk_size}", v.source.location),
        None => format!("{} · {disk_size}", v.source.location),
    };
    ui.label(RichText::new(what).color(theme::TEXT_DIM));
    ui.label(
        RichText::new("The whole disk is copied (every partition), into a new file; only data is stored, not zeros.")
            .color(theme::TEXT_DIM),
    );
    ui.add_space(6.0);
    let list = targets(ws);
    if let Some(job) = &mut d.running {
        let done = job.done.load(Ordering::Relaxed);
        let frac = if job.total == 0 { 1.0 } else { done as f32 / job.total as f32 };
        ui.add(egui::ProgressBar::new(frac).show_percentage());
        ui.label(format!("{} of {}", super::format_size(done), super::format_size(job.total)));
        match job.result.try_recv() {
            Ok(Ok((name, written))) => {
                actions.push(Action::Info(format!("Converted into {name} ({} of data)", super::format_size(written))));
                *close = true;
            }
            Ok(Err(e)) => {
                d.error = Some(e);
                d.running = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                super::dialogs::buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            Err(_) => {
                d.error = Some("the conversion ended without a result".into());
                d.running = None;
            }
        }
        return;
    }
    egui::Grid::new("convert_opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Format:");
        let current = list.iter().find(|t| t.0 == d.format).map(|t| t.2).unwrap_or("");
        let name = d.format.clone().unwrap_or_else(|| "Raw".into());
        egui::ComboBox::from_id_salt("convert_format")
            .selected_text(format!("{name} — {current}"))
            .width(360.0)
            .show_ui(ui, |ui| {
                for (f, ext, about) in &list {
                    let label = format!("{} (.{ext}) — {about}", f.as_deref().unwrap_or("Raw"));
                    ui.selectable_value(&mut d.format, f.clone(), label);
                }
            });
        ui.end_row();
    });
    if let Some(e) = &d.error {
        ui.colored_label(theme::DANGER, e);
    }
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let label = if cfg!(target_arch = "wasm32") { "Convert and download" } else { "Convert…" };
        if ui.button(label).clicked() {
            d.error = None;
            actions.push(Action::StartConvert);
        }
    });
}
