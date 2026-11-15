//! Left panel: open images and the folder tree of the current image.

use super::icons::{self, Glyph};
use super::{theme, Action, UiState};
use crate::model::{Volume, VolumeId, Workspace};
use dix_core::VfsPath;
use eframe::egui::{self, pos2, vec2, Painter, Rect, Response, RichText, Sense, Ui};
use std::collections::HashSet;
use std::sync::Arc;

const ROW_H: f32 = 24.0;
const INDENT: f32 = 16.0;

/// A sidebar row: optional expander, icon and label. Returns (row, expander).
fn row(
    ui: &mut Ui,
    depth: usize,
    selected: bool,
    expander: Option<bool>,
    label: &str,
    icon: impl FnOnce(&Painter, Rect),
    right_inset: f32,
) -> (Response, Option<Response>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let x = rect.left() + 4.0 + depth as f32 * INDENT;
    let exp_rect = Rect::from_min_size(pos2(x, rect.top()), vec2(16.0, ROW_H));
    let exp_resp = expander.map(|_| ui.interact(exp_rect, resp.id.with("exp"), Sense::click()));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, 5.0, theme::ACCENT);
        } else if resp.hovered() {
            p.rect_filled(rect, 5.0, theme::ROW_HOVER);
        }
        if let Some(open) = expander {
            icons::glyph(p, exp_rect, if open { Glyph::TriangleDown } else { Glyph::TriangleRight }, theme::TEXT_DIM);
        }
        let icon_rect = Rect::from_min_size(pos2(x + 18.0, rect.center().y - 9.0), vec2(18.0, 18.0));
        icon(p, icon_rect);
        let text_pos = pos2(icon_rect.right() + 7.0, rect.center().y);
        p.with_clip_rect(rect.with_max_x(rect.right() - right_inset)).text(
            text_pos,
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            theme::TEXT,
        );
    }
    (resp, exp_resp)
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).size(11.5).color(theme::TEXT_DIM));
}

pub fn show(ui: &mut Ui, ws: &mut Workspace, state: &mut UiState, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing.y = 1.0;
    heading(ui, "Images");
    let current_vol = ws.tab().map(|t| t.volume);
    egui::ScrollArea::vertical().id_salt("images").max_height(ui.available_height() * 0.35).show(ui, |ui| {
        for vol in ws.volumes.values() {
            let mut label = vol.name.clone();
            if vol.modified && vol.buffer.is_some() {
                label.push_str(" •");
            }
            let (resp, _) = row(ui, 0, Some(vol.id) == current_vol, None, &label, icons::disk, 28.0);
            let id = vol.id;
            // Eject button at the right end of the row.
            let eject_rect = Rect::from_center_size(resp.rect.right_center() - vec2(15.0, 0.0), vec2(22.0, 20.0));
            let eject = ui.interact(eject_rect, resp.id.with("eject"), Sense::click());
            if eject.hovered() {
                ui.painter().rect_filled(eject_rect, 4.0, egui::Color32::from_white_alpha(40));
            }
            let busy = ws.activities(id).len();
            let color = if eject.hovered() { egui::Color32::WHITE } else { theme::TEXT_DIM };
            icons::glyph(ui.painter(), eject_rect, Glyph::Eject, color);
            let tip = if busy > 0 { "Eject (busy: writing)" } else { "Eject (clean unmount)" };
            if eject.on_hover_text(tip).clicked() {
                actions.push(Action::Eject(id));
            }
            if resp.clicked() {
                match ws.tabs.iter().position(|t| t.volume == id) {
                    Some(i) => actions.push(Action::SelectTab(i)),
                    None => actions.push(Action::NewTabForVolume(id)),
                }
            }
            let hover = format!(
                "{}\n{} · {} · {}",
                vol.source.location,
                vol.driver.name,
                vol.mode().label(),
                super::format_size(vol.source.size)
            );
            let has_buffer = vol.buffer.is_some();
            let can_check = vol.driver.capabilities.contains(dix_core::Capabilities::CHECK);
            let settings = vol.has_settings();
            let password = vol.uses_passwords();
            let recoverable = dix_drivers::recover::supports(vol.driver.id);
            resp.on_hover_text(hover).context_menu(|ui| {
                volume_menu(ui, id, has_buffer, can_check, settings, password, recoverable, actions)
            });
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.small_button("Open image…").clicked() {
            actions.push(Action::ShowOpenDialog);
        }
        if ui.small_button("New image…").clicked() {
            actions.push(Action::ShowNewImageDialog);
        }
    });
    #[cfg(not(target_arch = "wasm32"))]
    if ui
        .small_button("Copy disk…")
        .on_hover_text(
            "Save a disk as an image (also from a failing disk), or write an image to a USB stick or SD card",
        )
        .clicked()
    {
        actions.push(Action::ShowCopyDisk);
    }

    let Some((tab, vol)) = ws.current() else { return };
    heading(ui, "Folders");
    let current = tab.path.clone();
    let rows = flatten_tree(vol, &state.expanded);
    // Only the visible rows are laid out, so huge expanded folders stay cheap.
    egui::ScrollArea::vertical().id_salt("folder_tree").auto_shrink([false, false]).show_rows(
        ui,
        ROW_H,
        rows.len(),
        |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for r in &rows[range] {
                tree_row(ui, vol, r, &current, &mut state.expanded, actions);
            }
        },
    );
}

/// One visible line of the folder tree.
struct TreeRow {
    depth: usize,
    path: VfsPath,
    /// `Some(open)` if an expander is shown.
    expander: Option<bool>,
}

/// Turns the expanded part of the tree into a flat list of rows.
///
/// Only expanded folders are listed. Collapsed folders show an expander
/// unless they are already known to be empty, so expanding a folder with
/// thousands of children does not read every child folder.
fn flatten_tree(vol: &mut Volume, expanded: &HashSet<(VolumeId, VfsPath)>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut stack = vec![(VfsPath::root(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        let open = expanded.contains(&(vol.id, path.clone()));
        let expander = if open {
            let children = vol.subdirs(&path).clone().unwrap_or_else(|_| Arc::from(Vec::new()));
            for name in children.iter().rev() {
                if let Ok(child) = path.join(name) {
                    stack.push((child, depth + 1));
                }
            }
            (!children.is_empty()).then_some(true)
        } else {
            match vol.cached_subdirs(&path) {
                Some(Ok(c)) if c.is_empty() => None,
                Some(Err(_)) => None,
                _ => Some(false),
            }
        };
        rows.push(TreeRow { depth, path, expander });
    }
    rows
}

fn tree_row(
    ui: &mut Ui,
    vol: &Volume,
    r: &TreeRow,
    current: &VfsPath,
    expanded: &mut HashSet<(VolumeId, VfsPath)>,
    actions: &mut Vec<Action>,
) {
    let label = r.path.file_name().unwrap_or(&vol.name);
    let is_root = r.path.is_root();
    let (resp, exp) = row(
        ui,
        r.depth,
        &r.path == current,
        r.expander,
        label,
        |p, rect| {
            if is_root {
                icons::disk(p, rect)
            } else {
                icons::folder(p, rect)
            }
        },
        0.0,
    );
    let key = (vol.id, r.path.clone());
    let toggle = |expanded: &mut HashSet<_>| {
        if !expanded.remove(&key) {
            expanded.insert(key.clone());
        }
    };
    if exp.is_some_and(|e| e.clicked()) || (resp.double_clicked() && r.expander.is_some()) {
        toggle(expanded);
    } else if resp.clicked() {
        actions.push(Action::Navigate(r.path.clone()));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn volume_menu(
    ui: &mut Ui,
    id: VolumeId,
    has_buffer: bool,
    can_check: bool,
    settings: bool,
    password: bool,
    recoverable: bool,
    actions: &mut Vec<Action>,
) {
    if ui.button("Open in new tab").clicked() {
        actions.push(Action::NewTabForVolume(id));
    }
    if ui.button("Properties").clicked() {
        actions.push(Action::ShowVolumeInfo(id));
    }
    if settings && ui.button("Settings…").on_hover_text("Label, how the image boots, …").clicked() {
        actions.push(Action::ShowSettings(id));
    }
    if password && ui.button("Enter password…").on_hover_text("For its encrypted files").clicked() {
        actions.push(Action::ShowPassword(id));
    }
    if recoverable
        && ui
            .button("Recover deleted files…")
            .on_hover_text("Look for files deleted from this volume; they open as a read-only volume to export from")
            .clicked()
    {
        actions.push(Action::RecoverDeleted(id));
    }
    if ui
        .button("Scan for photos and videos…")
        .on_hover_text("Find photos and videos by their content, even ones the file system no longer lists")
        .clicked()
    {
        actions.push(Action::ScanPhotos(id));
    }
    if ui
        .button("Convert disk image…")
        .on_hover_text("Save the whole disk as raw, qcow2, VHD, VHDX, VMDK, VDI or DMG")
        .clicked()
    {
        actions.push(Action::ShowConvert(id));
    }
    if ui.button("View raw hex").on_hover_text("The image's bytes (or its partition's)").clicked() {
        actions.push(Action::HexVolume(id));
    }
    if has_buffer {
        let label = if cfg!(target_arch = "wasm32") { "Download image" } else { "Save image as…" };
        if ui.button(label).clicked() {
            actions.push(Action::SaveImage(id));
        }
    }
    if can_check
        && ui
            .button("Check file system…")
            .on_hover_text("Look for inconsistencies (read-only, like fsck -n). The image is ejected and opened again.")
            .clicked()
    {
        actions.push(Action::CheckVolume(id));
    }
    ui.separator();
    if ui.button("Eject").on_hover_text("Clean unmount").clicked() {
        actions.push(Action::Eject(id));
    }
    if ui
        .button(RichText::new("Force Eject…").color(theme::DANGER))
        .on_hover_text("Unclean unmount: disconnect immediately, like pulling out the disk (for testing recovery)")
        .clicked()
    {
        actions.push(Action::WarnForceEject(id));
    }
}
