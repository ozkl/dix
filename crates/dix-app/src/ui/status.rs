//! Bottom status bar.

use super::theme;
use crate::model::Workspace;
use eframe::egui::{self, RichText, Ui};

pub fn show(ui: &mut Ui, ws: &Workspace, copy: Option<String>, actions: &mut Vec<super::Action>) {
    ui.horizontal_centered(|ui| {
        // A disk copy running in the background.
        if let Some(line) = copy {
            ui.add(egui::Spinner::new().size(14.0));
            ui.label(line);
            if ui.small_button("Show").clicked() {
                actions.push(super::Action::ShowHiddenCopy);
            }
            ui.separator();
        }
        // Running write jobs come first: they matter most (eject, quit).
        if let Some(job) = ws.jobs.iter().find(|j| j.handle.end().is_none()) {
            ui.add(egui::Spinner::new().size(14.0));
            let p = job.handle.progress();
            let more = if ws.jobs.len() > 1 { format!("  (+{} more)", ws.jobs.len() - 1) } else { String::new() };
            ui.label(format!("{} — {} items, {}{more}", job.title, p.items, super::format_size(p.bytes)))
                .on_hover_text(p.current.clone());
            if ui.small_button("Cancel").on_hover_text("Stop after the current file (clean stop)").clicked() {
                actions.push(super::Action::CancelJob(job.id));
            }
            ui.separator();
        }
        let Some(tab) = ws.tab() else {
            ui.label(RichText::new("No image open").color(theme::TEXT_DIM));
            return;
        };
        if let Some(search) = &tab.search {
            let n = tab.hit_count();
            let what = if search.mode == crate::model::SearchMode::Name { "items" } else { "files" };
            let text = if search.is_running() {
                format!("Searching… {n} found, {} {what} checked", search.scanned())
            } else {
                format!("{n} result{} ({} {what} checked)", if n == 1 { "" } else { "s" }, search.scanned())
            };
            ui.label(text);
            return;
        }
        let visible = tab.visible_len();
        let hidden = tab.total_len() - visible;
        let mut left = format!("{visible} item{}", if visible == 1 { "" } else { "s" });
        if hidden > 0 {
            left.push_str(&format!(" ({hidden} hidden)"));
        }
        ui.label(left);
        let sel: Vec<_> = tab.selected_entries().collect();
        if !sel.is_empty() {
            ui.separator();
            let bytes: u64 = sel.iter().filter(|e| !e.metadata.is_dir()).map(|e| e.metadata.size).sum();
            let mut s = format!("{} selected", sel.len());
            if bytes > 0 {
                s.push_str(&format!("  {}", super::format_size(bytes)));
            }
            ui.label(s);
        }
        if let Some(v) = ws.volumes.get(&tab.volume) {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let info = v.info();
                let mut parts = vec![info.fs_name.clone()];
                if let Some(l) = &info.label {
                    parts.push(format!("\"{l}\""));
                }
                if let (Some(free), Some(total)) = (info.free_bytes, info.total_bytes) {
                    parts.push(format!("{} free of {}", super::format_size(free), super::format_size(total)));
                }
                parts.push(v.mode().label().to_owned());
                ui.label(RichText::new(parts.join("  ·  ")).color(theme::TEXT_DIM));
            });
        }
    });
}
