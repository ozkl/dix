//! Tab strip, navigation/address bar and command bar.

use super::icons::{self, Glyph};
use super::{theme, Action, UiState, ViewMode};
use crate::model::{SearchMode, SortKey, Workspace};
use dix_core::{Capabilities, VfsPath};
use eframe::egui::{self, vec2, Button, Color32, FontId, Frame, RichText, Sense, Stroke, TextEdit, Ui};

pub fn tab_strip(ui: &mut Ui, ws: &Workspace, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, tab) in ws.tabs.iter().enumerate() {
            let Some(vol) = ws.volumes.get(&tab.volume) else { continue };
            let title = tab.title(&vol.name);
            let active = i == ws.active;
            let galley = ui.painter().layout_no_wrap(title, FontId::proportional(14.0), theme::TEXT);
            let w = (galley.size().x + 46.0).clamp(96.0, 230.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
            let close_rect = egui::Rect::from_center_size(rect.right_center() - vec2(16.0, 0.0), vec2(18.0, 18.0));
            let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());
            let fill = if active {
                theme::ACCENT
            } else if resp.hovered() {
                Color32::from_rgb(52, 52, 52)
            } else {
                Color32::from_rgb(42, 42, 42)
            };
            let p = ui.painter();
            p.rect_filled(rect, 6.0, fill);
            let text_rect =
                egui::Rect::from_min_max(rect.min + vec2(12.0, 0.0), close_rect.left_bottom() - vec2(4.0, 0.0));
            p.with_clip_rect(text_rect.intersect(rect)).galley(
                egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0),
                galley,
                theme::TEXT,
            );
            if close.hovered() {
                p.rect_filled(close_rect, 4.0, Color32::from_white_alpha(30));
            }
            icons::glyph(
                p,
                close_rect,
                Glyph::Close,
                if active || resp.hovered() { theme::TEXT } else { theme::TEXT_DIM },
            );
            if close.clicked() || resp.middle_clicked() {
                actions.push(Action::CloseTab(i));
            } else if resp.clicked() {
                actions.push(Action::SelectTab(i));
            }
            resp.on_hover_text(format!("{} — {}", vol.name, tab.path));
        }
        if icons::button(ui, Glyph::Plus, true, "Open an image (Ctrl+O)").clicked() {
            actions.push(Action::ShowOpenDialog);
        }
    });
}

pub fn nav_bar(ui: &mut Ui, ws: &mut Workspace, state: &mut UiState, actions: &mut Vec<Action>) {
    let has_tab = ws.tab().is_some();
    let (can_back, can_fwd, can_up) =
        ws.tab().map(|t| (t.can_back(), t.can_forward(), !t.path.is_root())).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if icons::button(ui, Glyph::Back, can_back, "Back (Alt+Left)").clicked() {
            actions.push(Action::Back);
        }
        if icons::button(ui, Glyph::Forward, can_fwd, "Forward (Alt+Right)").clicked() {
            actions.push(Action::Forward);
        }
        if icons::button(ui, Glyph::Up, can_up, "Up (Alt+Up)").clicked() {
            actions.push(Action::Up);
        }
        if icons::button(ui, Glyph::Refresh, has_tab, "Refresh (F5)").clicked() {
            actions.push(Action::Refresh);
        }
        ui.add_space(6.0);
        let search_w = (ui.available_width() * 0.28).clamp(140.0, 300.0);
        let addr_w = ui.available_width() - search_w - 8.0;
        address_bar(ui, ws, state, addr_w, actions);
        ui.add_space(6.0);
        search_box(ui, ws, state, search_w);
    });
}

fn address_bar(ui: &mut Ui, ws: &mut Workspace, state: &mut UiState, width: f32, actions: &mut Vec<Action>) {
    let frame = Frame::new()
        .fill(theme::FIELD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(5.0)
        .inner_margin(egui::Margin::symmetric(8, 2));
    frame.show(ui, |ui| {
        ui.set_width(width - 18.0);
        ui.set_height(24.0);
        let Some(tab) = ws.tab() else {
            ui.label(RichText::new("No image open").color(theme::TEXT_DIM));
            return;
        };
        let vol_name = ws.volumes.get(&tab.volume).map(|v| v.name.clone()).unwrap_or_default();
        let path = tab.path.clone();

        if let Some(buf) = &mut state.address_edit {
            let resp = ui.add(
                bar_field(TextEdit::singleline(buf)).font(FontId::proportional(14.5)).desired_width(f32::INFINITY),
            );
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
            }
            if resp.lost_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    match VfsPath::parse(buf) {
                        Ok(p) => actions.push(Action::Navigate(p)),
                        Err(e) => state.error(e.to_string()),
                    }
                }
                state.address_edit = None;
            }
            return;
        }

        let vol_id = tab.volume;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let ancestors = path.ancestors_inclusive();
            // Show at most the last 6 levels; earlier ones collapse into "…".
            let skip = ancestors.len().saturating_sub(6);
            for (i, p) in ancestors.iter().enumerate() {
                if i > 0 && i < skip {
                    continue;
                }
                if i > 0 && i == skip {
                    ui.label(RichText::new("…").color(theme::TEXT_DIM));
                }
                let label = if p.is_root() { vol_name.as_str() } else { p.file_name().unwrap_or("") };
                if crumb(ui, label).on_hover_text(p.to_string()).clicked() {
                    actions.push(Action::Navigate(p.clone()));
                }
                let chev = chevron(ui);
                egui::Popup::menu(&chev).show(|ui| {
                    ui.set_min_width(160.0);
                    let subdirs = match ws.volumes.get_mut(&vol_id).map(|v| v.subdirs(p).clone()) {
                        Some(Ok(d)) => d,
                        Some(Err(e)) => {
                            ui.colored_label(theme::DANGER, e.to_string());
                            return;
                        }
                        None => return,
                    };
                    if subdirs.is_empty() {
                        ui.label(RichText::new("No subfolders").color(theme::TEXT_DIM));
                    }
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for name in subdirs.iter() {
                            if ui.button(RichText::new(name).size(14.5)).clicked() {
                                if let Ok(child) = p.join(name) {
                                    actions.push(Action::Navigate(child));
                                }
                            }
                        }
                    });
                });
            }
            // The empty rest of the bar switches to text editing.
            let rest = ui.available_rect_before_wrap();
            let r = ui.interact(rest, ui.scope_id().with("address_rest"), Sense::click());
            if r.on_hover_cursor(egui::CursorIcon::Text).clicked() {
                state.address_edit = Some(path.to_string());
            }
        });
    });
}

/// Highlight used by breadcrumb parts: grey fill with a light border.
fn paint_crumb_bg(ui: &Ui, rect: egui::Rect, strong: bool) {
    let fill = if strong { Color32::from_rgb(72, 72, 72) } else { Color32::from_rgb(56, 56, 56) };
    ui.painter().rect(rect, 5.0, fill, Stroke::new(1.0, Color32::from_rgb(150, 150, 150)), egui::StrokeKind::Inside);
}

/// One clickable path component.
fn crumb(ui: &mut Ui, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(14.5), theme::TEXT);
    let size = vec2(galley.size().x + 14.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        paint_crumb_bg(ui, rect, resp.is_pointer_button_down_on());
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, theme::TEXT);
    resp
}

/// The "›" after a component; clicking it lists that folder's sub folders.
fn chevron(ui: &mut Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(20.0, 22.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if resp.hovered() || open {
        paint_crumb_bg(ui, rect, open);
    }
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "›", FontId::proportional(17.0), theme::TEXT);
    resp
}

/// A frameless text field that fills the 24 px bar height with its text
/// vertically centred (by default the text sits at the top of the frame).
fn bar_field(edit: TextEdit<'_>) -> TextEdit<'_> {
    edit.frame(Frame::NONE).margin(egui::Margin::ZERO).min_size(vec2(0.0, 24.0)).align(egui::Align2::LEFT_CENTER)
}

fn search_box(ui: &mut Ui, ws: &mut Workspace, state: &mut UiState, width: f32) {
    let frame = Frame::new()
        .fill(theme::FIELD_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(5.0)
        .inner_margin(egui::Margin { left: 8, right: 4, top: 2, bottom: 2 });
    let Some(tab) = ws.tabs.get_mut(ws.active) else {
        frame.show(ui, |ui| {
            ui.set_width(width - 12.0);
            ui.set_height(24.0);
            ui.add_enabled(false, bar_field(TextEdit::singleline(&mut String::new()).hint_text("Search")));
        });
        return;
    };
    frame.show(ui, |ui| {
        ui.set_width(width - 12.0);
        ui.set_height(24.0);
        ui.horizontal_centered(|ui| {
            let mode = tab.search_mode;
            let toggle_text = RichText::new(mode.label()).size(12.0).color(Color32::WHITE);
            let toggle_w =
                ui.painter().layout_no_wrap(mode.label().into(), FontId::proportional(12.0), Color32::WHITE).size().x
                    + 16.0;
            let edit = ui.add(
                bar_field(TextEdit::singleline(&mut tab.search_query).hint_text("Search"))
                    .desired_width(ui.available_width() - toggle_w - 6.0),
            );
            if edit.changed() {
                tab.search_pending = true;
                state.search_changed_at = state.now;
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                tab.search_pending = true;
                state.search_changed_at = f64::NEG_INFINITY; // start right away
            }
            if edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                tab.clear_search();
            }
            let tip = match mode {
                SearchMode::Name => "Searching by file name — click to search file contents",
                SearchMode::Content => "Searching file contents — click to search by file name",
            };
            let toggle =
                ui.add(Button::new(toggle_text).frame(false).min_size(vec2(toggle_w, 20.0))).on_hover_text(tip);
            if toggle.clicked() {
                tab.search_mode = mode.toggled();
                tab.search_pending = true;
                state.search_changed_at = f64::NEG_INFINITY;
            }
        });
    });
}

pub fn command_bar(
    ui: &mut Ui,
    ws: &mut Workspace,
    state: &mut UiState,
    can_import_folders: bool,
    actions: &mut Vec<Action>,
) {
    let vol = ws.current_volume();
    let has_vol = vol.is_some();
    let caps = vol.map(|v| v.caps()).unwrap_or_default();
    let read_only = vol.is_some_and(|v| !v.mode().is_writable());
    let driver_name = vol.map(|v| v.driver.name).unwrap_or("");
    let has_tab = ws.tab().is_some();
    let sel = ws.tab().map(|t| t.selection.len()).unwrap_or(0);
    let has_clip = ws.clipboard.is_some();

    let why = |cap: Capabilities| -> String {
        if !has_vol {
            "No image open".into()
        } else if read_only && cap.is_writable() {
            "The image is opened read-only".into()
        } else if !caps.contains(cap) {
            format!("Not supported by the {driver_name} driver")
        } else {
            "Select something first".into()
        }
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let cmd = |ui: &mut Ui,
                   label: &str,
                   enabled: bool,
                   reason: String,
                   hint: &str,
                   action: Action,
                   actions: &mut Vec<Action>| {
            let r = ui.add_enabled(enabled, Button::new(label));
            if r.on_hover_text(hint).on_disabled_hover_text(reason).clicked() {
                actions.push(action);
            }
        };
        cmd(
            ui,
            "New folder",
            caps.contains(Capabilities::CREATE_DIR),
            why(Capabilities::CREATE_DIR),
            "Ctrl+Shift+N",
            Action::NewFolder,
            actions,
        );
        ui.separator();
        cmd(
            ui,
            "Cut",
            sel > 0 && caps.contains(Capabilities::REMOVE),
            why(Capabilities::REMOVE),
            "Ctrl+X",
            Action::Cut,
            actions,
        );
        cmd(ui, "Copy", sel > 0, why(Capabilities::READ), "Ctrl+C", Action::Copy, actions);
        cmd(
            ui,
            "Paste",
            has_clip && caps.contains(Capabilities::WRITE),
            if has_clip { why(Capabilities::WRITE) } else { "The clipboard is empty".into() },
            "Ctrl+V",
            Action::Paste,
            actions,
        );
        cmd(
            ui,
            "Rename",
            sel == 1 && caps.contains(Capabilities::RENAME),
            why(Capabilities::RENAME),
            "F2",
            Action::BeginRename,
            actions,
        );
        cmd(
            ui,
            "Delete",
            sel > 0 && caps.contains(Capabilities::REMOVE),
            why(Capabilities::REMOVE),
            "Del",
            Action::RequestDelete,
            actions,
        );
        ui.separator();

        let can_import = caps.contains(Capabilities::WRITE);
        ui.add_enabled_ui(can_import, |ui| {
            ui.menu_button("Import", |ui| {
                if ui.button("Files…").clicked() {
                    actions.push(Action::ImportFiles);
                }
                if can_import_folders && ui.button("Folder…").clicked() {
                    actions.push(Action::ImportFolder);
                }
            })
            .response
            .on_hover_text("Copy files from this computer into the image")
            .on_disabled_hover_text(why(Capabilities::WRITE));
        });
        cmd(
            ui,
            "Export",
            sel > 0,
            why(Capabilities::READ),
            "Save the selection to this computer",
            Action::Export,
            actions,
        );
        ui.separator();

        ui.add_enabled_ui(has_tab, |ui| {
            ui.menu_button("Sort", |ui| {
                if let Some(t) = ws.tabs.get_mut(ws.active) {
                    for key in SortKey::ALL {
                        if ui.radio(t.sort.key == key, key.label()).clicked() {
                            t.sort.key = key;
                            t.rebuild_view();
                        }
                    }
                    ui.separator();
                    if ui.radio(t.sort.ascending, "Ascending").clicked() {
                        t.sort.ascending = true;
                        t.rebuild_view();
                    }
                    if ui.radio(!t.sort.ascending, "Descending").clicked() {
                        t.sort.ascending = false;
                        t.rebuild_view();
                    }
                    ui.separator();
                    if ui.checkbox(&mut t.sort.folders_first, "Folders first").changed() {
                        t.rebuild_view();
                    }
                }
            });
            ui.menu_button("View", |ui| {
                ui.radio_value(&mut state.view, ViewMode::Details, "Details");
                ui.radio_value(&mut state.view, ViewMode::Icons, "Large icons");
                ui.separator();
                if ui.button("Licenses…").clicked() {
                    state.licenses = true;
                    ui.close();
                }
            });
            if let Some(t) = ws.tabs.get_mut(ws.active) {
                if ui.checkbox(&mut t.show_hidden, "Show hidden").changed() {
                    t.rebuild_view();
                }
            }
            ui.checkbox(&mut state.show_preview, "Preview");
        });

        if let Some(v) = ws.current_volume() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color) =
                    if v.mode().is_writable() { ("Read-write", theme::WARN) } else { ("Read-only", theme::OK) };
                let r = Frame::new()
                    .stroke(Stroke::new(1.0, color))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(8, 1))
                    .show(ui, |ui| ui.label(RichText::new(text).color(color).size(12.0)))
                    .response;
                let id = v.id;
                if r.interact(Sense::click())
                    .on_hover_text(format!("{} · {} — click for details", v.driver.name, v.source.kind.label()))
                    .clicked()
                {
                    actions.push(Action::ShowVolumeInfo(id));
                }
            });
        }
    });
}
