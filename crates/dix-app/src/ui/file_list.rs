//! The central file list: "Details" table and "Large icons" grid.

use super::{icons, theme, Action, RenameState, UiState, ViewMode};
use crate::model::{SortKey, Tab, Workspace};
use dix_core::{Capabilities, DirEntry};
use eframe::egui::{
    self, pos2, text::LayoutJob, vec2, Align2, Button, Color32, FontId, Rect, Response, RichText, Sense, Stroke,
    TextEdit, TextFormat, Ui,
};
use std::collections::HashSet;

const ROW_H: f32 = 26.0;
const HEADER_H: f32 = 30.0;
const TILE: egui::Vec2 = egui::Vec2::new(108.0, 100.0);

/// Information the list needs besides the tab itself.
struct ListCtx {
    caps: Capabilities,
    has_clipboard: bool,
    /// Names in the current directory that are "cut" (drawn faded).
    cut_names: HashSet<String>,
    can_import_folders: bool,
    /// The folder shown.
    dir: dix_core::VfsPath,
}

pub fn show(ui: &mut Ui, ws: &mut Workspace, state: &mut UiState, can_import_folders: bool, actions: &mut Vec<Action>) {
    let caps = ws.current_volume().map(|v| v.caps()).unwrap_or_default();
    let cut_names = match (&ws.clipboard, ws.tab()) {
        (Some(c), Some(t)) if c.cut && c.volume == t.volume => c
            .paths
            .iter()
            .filter(|p| p.parent().as_ref() == Some(&t.path))
            .filter_map(|p| p.file_name().map(str::to_owned))
            .collect(),
        _ => HashSet::new(),
    };
    let dir = ws.tab().map(|t| t.path.clone()).unwrap_or_else(dix_core::VfsPath::root);
    let lctx = ListCtx { caps, has_clipboard: ws.clipboard.is_some(), cut_names, can_import_folders, dir };
    let Some(tab) = ws.tabs.get_mut(ws.active) else { return };

    if let Some(err) = &tab.error {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.colored_label(theme::DANGER, format!("Cannot read {}: {err}", tab.path)));
        return;
    }

    if tab.search.is_some() {
        search_results(ui, tab, state, actions);
        return;
    }

    let full = ui.available_rect_before_wrap();
    let bg = ui.interact(full, ui.scope_id().with("list_background"), Sense::click());
    if bg.clicked() {
        tab.selection.clear();
        tab.anchor = None;
    }
    bg.context_menu(|ui| background_menu(ui, &lctx, actions));

    match state.view {
        ViewMode::Details => details(ui, tab, &lctx, state, actions),
        ViewMode::Icons => icon_grid(ui, tab, &lctx, state, actions),
    }

    if tab.visible_len() == 0 {
        ui.painter().text(
            full.center_top() + vec2(0.0, 80.0),
            Align2::CENTER_CENTER,
            "This folder is empty.",
            FontId::proportional(14.0),
            theme::TEXT_DIM,
        );
    }
}

// ------------------------------------------------------------------ details

struct Columns {
    name: (f32, f32),
    date: (f32, f32),
    kind: (f32, f32),
    size: (f32, f32),
}

impl Columns {
    fn new(left: f32, width: f32, widths: [f32; 3]) -> Self {
        let [dw, tw, sw] = widths;
        let name_w = (width - dw - tw - sw - 12.0).max(140.0);
        let x0 = left + 8.0;
        let x1 = x0 + name_w;
        let x2 = x1 + dw;
        let x3 = x2 + tw;
        Self { name: (x0, x1), date: (x1, x2), kind: (x2, x3), size: (x3, x3 + sw) }
    }
}

fn details(ui: &mut Ui, tab: &mut Tab, lctx: &ListCtx, state: &mut UiState, actions: &mut Vec<Action>) {
    let full = ui.available_rect_before_wrap();
    let cols = Columns::new(full.left(), full.width(), state.columns);

    // Header with sortable, resizable columns.
    let (header, _) = ui.allocate_exact_size(vec2(full.width(), HEADER_H), Sense::hover());
    let headers = [
        (SortKey::Name, cols.name, false),
        (SortKey::Modified, cols.date, false),
        (SortKey::Type, cols.kind, false),
        (SortKey::Size, cols.size, true),
    ];
    for (key, (x0, x1), right) in headers {
        let r = Rect::from_x_y_ranges(x0..=x1, header.y_range());
        let resp = ui.interact(r, ui.scope_id().with(("hdr", key.label())), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, 3.0, theme::ROW_HOVER);
        }
        if resp.clicked() {
            tab.set_sort(key);
        }
        let p = ui.painter().with_clip_rect(r);
        let font = FontId::proportional(14.5);
        let tx = if right { x1 - 10.0 } else { x0 + 10.0 };
        let align = if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
        let text_rect = p.text(pos2(tx, r.center().y), align, key.label(), font, theme::TEXT);
        if tab.sort.key == key {
            let ix = if right { text_rect.left() - 10.0 } else { text_rect.right() + 10.0 };
            let ir = Rect::from_center_size(pos2(ix, r.center().y), vec2(12.0, 12.0));
            if tab.sort.ascending {
                up_triangle(&p, ir);
            } else {
                icons::glyph(&p, ir, icons::Glyph::TriangleDown, theme::TEXT_DIM);
            }
        }
    }
    // Resize handles between columns.
    let handles = [(cols.date.0, 0, -1.0), (cols.kind.0, 0, 1.0), (cols.size.0, 1, 1.0)];
    for (x, idx, sign) in handles {
        let r = Rect::from_center_size(pos2(x, header.center().y), vec2(8.0, HEADER_H));
        let resp = ui.interact(r, ui.scope_id().with(("col_resize", x as i32, idx)), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if resp.dragged() {
            let w = &mut state.columns[idx];
            *w = (*w + sign * resp.drag_delta().x).clamp(60.0, 600.0);
        }
    }
    ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, theme::SEPARATOR));

    // Column separators across the list (as in the screenshot).
    let body_top = header.bottom();
    for x in [cols.date.0, cols.kind.0, cols.size.0] {
        ui.painter().line_segment(
            [pos2(x - 4.0, body_top + 6.0), pos2(x - 4.0, full.bottom() - 4.0)],
            Stroke::new(1.0, theme::SEPARATOR),
        );
    }

    let n = tab.visible_len();
    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("details");
    if let Some(target) = state.scroll_to.take() {
        if let Some(off) = scroll_offset_for(target, ROW_H, state.list_scroll) {
            area = area.vertical_scroll_offset(off);
        }
    }
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, ROW_H, n, |ui, range| {
                for i in range {
                    let Some(entry) = tab.visible_at(i).cloned() else { continue };
                    detail_row(ui, i, &entry, &cols, tab, lctx, state, actions);
                }
            })
        })
        .inner;
    state.list_scroll = (out.state.clamped_offset().y, out.inner_rect.height());
}

fn up_triangle(p: &egui::Painter, r: Rect) {
    let c = r.center();
    let d = r.width() * 0.32;
    p.add(egui::Shape::convex_polygon(
        vec![c + vec2(-d, d * 0.6), c + vec2(d, d * 0.6), c + vec2(0.0, -d * 0.8)],
        theme::TEXT_DIM,
        Stroke::NONE,
    ));
}

/// Offset that makes row `target` visible, if it currently is not.
fn scroll_offset_for(target: usize, row_h: f32, (offset, view_h): (f32, f32)) -> Option<f32> {
    let top = target as f32 * row_h;
    if top < offset {
        Some(top)
    } else if top + row_h > offset + view_h && view_h > 0.0 {
        Some(top + row_h - view_h)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn detail_row(
    ui: &mut Ui,
    idx: usize,
    e: &DirEntry,
    cols: &Columns,
    tab: &mut Tab,
    lctx: &ListCtx,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let selected = tab.selection.contains(&e.name);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let bg = rect.shrink2(vec2(4.0, 1.0));
        if selected {
            p.rect_filled(bg, 4.0, theme::ACCENT);
        } else if resp.hovered() {
            p.rect_filled(bg, 4.0, theme::ROW_HOVER);
        }
        let faded = lctx.cut_names.contains(&e.name) || super::super::model::is_hidden(e);
        let text_color = if faded { theme::TEXT_DIM } else { theme::TEXT };
        let dim = if selected { Color32::from_gray(205) } else { theme::TEXT_DIM };
        let font = FontId::proportional(14.5);
        let cy = rect.center().y;

        let icon_rect = Rect::from_center_size(pos2(cols.name.0 + 14.0, cy), vec2(20.0, 20.0));
        icons::entry(p, icon_rect, e.metadata.kind);
        let clip = |x0: f32, x1: f32| Rect::from_x_y_ranges(x0..=x1 - 8.0, rect.y_range());
        if !is_renaming(state, &e.name) {
            let named = p.with_clip_rect(clip(cols.name.0, cols.name.1)).text(
                pos2(cols.name.0 + 34.0, cy),
                Align2::LEFT_CENTER,
                &e.name,
                font.clone(),
                text_color,
            );
            // A remark (a deleted file's chance), after the name.
            if let Some(note) = &e.metadata.note {
                let color = if selected {
                    dim
                } else if note.contains("intact") {
                    theme::OK
                } else if note.contains("lost") || note.ends_with("· overwritten") {
                    theme::DANGER
                } else if note.contains("overwritten") {
                    theme::WARN
                } else {
                    dim
                };
                p.with_clip_rect(clip(cols.name.0, cols.name.1)).text(
                    pos2(named.right() + 10.0, cy),
                    Align2::LEFT_CENTER,
                    note,
                    FontId::proportional(12.5),
                    color,
                );
            }
        }
        p.with_clip_rect(clip(cols.date.0, cols.date.1)).text(
            pos2(cols.date.0 + 6.0, cy),
            Align2::LEFT_CENTER,
            super::format_time(e.metadata.modified),
            font.clone(),
            dim,
        );
        p.with_clip_rect(clip(cols.kind.0, cols.kind.1)).text(
            pos2(cols.kind.0 + 6.0, cy),
            Align2::LEFT_CENTER,
            crate::model::tab_type_label(e),
            font.clone(),
            dim,
        );
        if !e.metadata.is_dir() {
            p.with_clip_rect(Rect::from_x_y_ranges(cols.size.0..=cols.size.1, rect.y_range())).text(
                pos2(cols.size.1 - 10.0, cy),
                Align2::RIGHT_CENTER,
                super::format_size(e.metadata.size),
                font,
                dim,
            );
        }
    }
    if is_renaming(state, &e.name) {
        let r = Rect::from_min_max(
            pos2(cols.name.0 + 30.0, rect.top() + 2.0),
            pos2(cols.name.1 - 8.0, rect.bottom() - 2.0),
        );
        rename_editor(ui, r, state, actions);
    }
    handle_entry_response(ui, &resp, idx, e, selected, tab, lctx, state, actions);
}

fn is_renaming(state: &UiState, name: &str) -> bool {
    state.renaming.as_ref().is_some_and(|r| r.original == name)
}

fn rename_editor(ui: &mut Ui, rect: Rect, state: &mut UiState, actions: &mut Vec<Action>) {
    let Some(rn) = state.renaming.as_mut() else { return };
    let resp = ui.put(rect, TextEdit::singleline(&mut rn.buffer).margin(egui::Margin::symmetric(4, 2)));
    if !rn.focus_requested {
        rn.focus_requested = true;
        resp.request_focus();
        // Pre-select the name without its extension.
        let stem = match rn.buffer.rfind('.') {
            Some(i) if i > 0 => rn.buffer[..i].chars().count(),
            _ => rn.buffer.chars().count(),
        };
        if let Some(mut s) = TextEdit::load_state(ui.ctx(), resp.id) {
            s.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(stem),
            )));
            s.store(ui.ctx(), resp.id);
        }
    } else if resp.lost_focus() {
        let rn = state.renaming.take().unwrap();
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) && rn.buffer != rn.original && !rn.buffer.trim().is_empty() {
            actions.push(Action::CommitRename { old: rn.original, new: rn.buffer.trim().to_owned() });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_entry_response(
    ui: &Ui,
    resp: &Response,
    idx: usize,
    e: &DirEntry,
    selected: bool,
    tab: &mut Tab,
    lctx: &ListCtx,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    if resp.double_clicked() {
        actions.push(Action::OpenEntry(e.name.clone()));
    } else if resp.clicked() {
        let m = ui.input(|i| i.modifiers);
        tab.click(idx, m.command, m.shift);
        if !is_renaming(state, &e.name) {
            state.renaming = None;
        }
    }
    if resp.secondary_clicked() && !selected {
        tab.select_only(&e.name);
    }
    let sel_count = tab.selection.len().max(1);
    resp.context_menu(|ui| entry_menu(ui, e, sel_count, lctx, actions));
}

fn menu_item(ui: &mut Ui, label: &str, shortcut: &str, enabled: bool, action: Action, actions: &mut Vec<Action>) {
    if ui.add_enabled(enabled, Button::new(label).shortcut_text(shortcut)).clicked() {
        actions.push(action);
    }
}

fn entry_menu(ui: &mut Ui, e: &DirEntry, sel: usize, lctx: &ListCtx, actions: &mut Vec<Action>) {
    let c = lctx.caps;
    let open = if e.metadata.is_dir() { "Open" } else { "Preview" };
    menu_item(ui, open, "Enter", true, Action::OpenEntry(e.name.clone()), actions);
    if e.metadata.kind == dix_core::EntryKind::Symlink {
        if let Ok(p) = lctx.dir.join(&e.name) {
            menu_item(ui, "Go to target", "", sel == 1, Action::GoToLinkTarget(p), actions);
        }
    }
    if e.metadata.kind == dix_core::EntryKind::File {
        menu_item(ui, "Open as image…", "", sel == 1, Action::OpenEntryAsImage(e.name.clone()), actions);
        menu_item(ui, "View hex", "", sel == 1, Action::HexFile(e.name.clone()), actions);
    }
    menu_item(ui, "Export…", "", true, Action::Export, actions);
    ui.separator();
    menu_item(ui, "Cut", "Ctrl+X", c.contains(Capabilities::REMOVE), Action::Cut, actions);
    menu_item(ui, "Copy", "Ctrl+C", true, Action::Copy, actions);
    menu_item(ui, "Rename", "F2", sel == 1 && c.contains(Capabilities::RENAME), Action::BeginRename, actions);
    menu_item(ui, "Delete", "Del", c.contains(Capabilities::REMOVE), Action::RequestDelete, actions);
    ui.separator();
    menu_item(ui, "Properties", "", true, Action::Properties, actions);
}

fn background_menu(ui: &mut Ui, lctx: &ListCtx, actions: &mut Vec<Action>) {
    let c = lctx.caps;
    menu_item(ui, "New folder", "Ctrl+Shift+N", c.contains(Capabilities::CREATE_DIR), Action::NewFolder, actions);
    menu_item(ui, "Paste", "Ctrl+V", lctx.has_clipboard && c.contains(Capabilities::WRITE), Action::Paste, actions);
    ui.separator();
    menu_item(ui, "Import files…", "", c.contains(Capabilities::WRITE), Action::ImportFiles, actions);
    if lctx.can_import_folders {
        menu_item(ui, "Import folder…", "", c.contains(Capabilities::WRITE), Action::ImportFolder, actions);
    }
    ui.separator();
    menu_item(ui, "Refresh", "F5", true, Action::Refresh, actions);
}

// -------------------------------------------------------------------- icons

fn icon_grid(ui: &mut Ui, tab: &mut Tab, lctx: &ListCtx, state: &mut UiState, actions: &mut Vec<Action>) {
    let width = ui.available_width();
    let per_row = ((width - 8.0) / TILE.x).floor().max(1.0) as usize;
    let n = tab.visible_len();
    let rows = n.div_ceil(per_row);
    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("icons");
    if let Some(target) = state.scroll_to.take() {
        if let Some(off) = scroll_offset_for(target / per_row, TILE.y, state.list_scroll) {
            area = area.vertical_scroll_offset(off);
        }
    }
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            area.show_rows(ui, TILE.y, rows, |ui, range| {
                for r in range {
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        for idx in r * per_row..((r + 1) * per_row).min(n) {
                            let Some(entry) = tab.visible_at(idx).cloned() else { continue };
                            tile(ui, idx, &entry, tab, lctx, state, actions);
                        }
                    });
                }
            })
        })
        .inner;
    state.list_scroll = (out.state.clamped_offset().y, out.inner_rect.height());
}

fn tile(
    ui: &mut Ui,
    idx: usize,
    e: &DirEntry,
    tab: &mut Tab,
    lctx: &ListCtx,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let (rect, resp) = ui.allocate_exact_size(TILE, Sense::click());
    let selected = tab.selection.contains(&e.name);
    let inner = rect.shrink(3.0);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect_filled(inner, 6.0, theme::ACCENT);
        } else if resp.hovered() {
            p.rect_filled(inner, 6.0, theme::ROW_HOVER);
        }
        let icon = Rect::from_center_size(pos2(rect.center().x, rect.top() + 32.0), vec2(48.0, 48.0));
        icons::entry(p, icon, e.metadata.kind);
        if !is_renaming(state, &e.name) {
            let faded = lctx.cut_names.contains(&e.name) || crate::model::is_hidden(e);
            let mut job = LayoutJob::single_section(
                e.name.clone(),
                TextFormat {
                    font_id: FontId::proportional(13.0),
                    color: if faded { theme::TEXT_DIM } else { theme::TEXT },
                    ..Default::default()
                },
            );
            job.wrap.max_width = TILE.x - 12.0;
            job.wrap.max_rows = 2;
            job.wrap.break_anywhere = true;
            job.halign = egui::Align::Center;
            let galley = p.layout_job(job);
            p.galley(pos2(rect.center().x, rect.top() + 60.0), galley, theme::TEXT);
        }
    }
    if is_renaming(state, &e.name) {
        let r = Rect::from_min_size(pos2(rect.left() + 4.0, rect.top() + 60.0), vec2(TILE.x - 8.0, 22.0));
        rename_editor(ui, r, state, actions);
    }
    let resp = resp.on_hover_text(RichText::new(format!(
        "{}\n{}{}",
        e.name,
        crate::model::tab_type_label(e),
        if e.metadata.is_dir() { String::new() } else { format!(" · {}", super::format_size(e.metadata.size)) }
    )));
    handle_entry_response(ui, &resp, idx, e, selected, tab, lctx, state, actions);
}

/// Starts inline renaming of `name`.
pub fn begin_rename(state: &mut UiState, name: &str) {
    state.renaming = Some(RenameState { original: name.to_owned(), buffer: name.to_owned(), focus_requested: false });
}

// ----------------------------------------------------------- search results

fn search_results(ui: &mut Ui, tab: &mut Tab, state: &mut UiState, actions: &mut Vec<Action>) {
    let Some(search) = &tab.search else { return };
    let (running, mode, query, root) = (search.is_running(), search.mode, search.query.clone(), search.root.clone());
    let n_hits = tab.hit_count();

    // Summary line.
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        if running {
            ui.add(egui::Spinner::new().size(14.0));
        }
        let what = match mode {
            crate::model::SearchMode::Name => "names matching",
            crate::model::SearchMode::Content => "files containing",
        };
        ui.label(RichText::new(format!("{what} \"{query}\" in {root}")).color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Clear search").clicked() {
                tab.clear_search();
            }
        });
    });
    if tab.search.is_none() {
        return;
    }

    let full = ui.available_rect_before_wrap();
    let w = full.width() - 16.0;
    let x0 = full.left() + 8.0;
    let cols = [x0, x0 + w * 0.36, x0 + w * 0.72, x0 + w * 0.86, x0 + w];
    let (header, _) = ui.allocate_exact_size(vec2(full.width(), HEADER_H), Sense::hover());
    let p = ui.painter();
    for (i, label) in ["Name", "Location", "Type", "Size"].into_iter().enumerate() {
        let right = i == 3;
        let pos = if right { pos2(cols[4] - 10.0, header.center().y) } else { pos2(cols[i] + 10.0, header.center().y) };
        p.text(
            pos,
            if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER },
            label,
            FontId::proportional(14.5),
            theme::TEXT,
        );
    }
    p.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, theme::SEPARATOR));
    for x in &cols[1..4] {
        p.line_segment(
            [pos2(x - 4.0, header.bottom() + 6.0), pos2(x - 4.0, full.bottom() - 4.0)],
            Stroke::new(1.0, theme::SEPARATOR),
        );
    }

    if n_hits == 0 {
        let msg = if running { "Searching…" } else { "No results." };
        ui.painter().text(
            full.center_top() + vec2(0.0, 90.0),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(14.0),
            theme::TEXT_DIM,
        );
        return;
    }

    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("search_results");
    if let Some(target) = state.scroll_to.take() {
        if let Some(off) = scroll_offset_for(target, ROW_H, state.list_scroll) {
            area = area.vertical_scroll_offset(off);
        }
    }
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, ROW_H, n_hits, |ui, range| {
                let mut clicked = None;
                for i in range {
                    // Only the visible rows are touched (no per-frame copies).
                    let Some(hit) = tab.hit(i) else { continue };
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                    let selected = tab.result_sel == Some(i);
                    if ui.is_rect_visible(rect) {
                        let p = ui.painter();
                        let bg = rect.shrink2(vec2(4.0, 1.0));
                        if selected {
                            p.rect_filled(bg, 4.0, theme::ACCENT);
                        } else if resp.hovered() {
                            p.rect_filled(bg, 4.0, theme::ROW_HOVER);
                        }
                        let cy = rect.center().y;
                        let font = FontId::proportional(14.5);
                        let dim = if selected { Color32::from_gray(205) } else { theme::TEXT_DIM };
                        let clip = |a: f32, b: f32| Rect::from_x_y_ranges(a..=b - 8.0, rect.y_range());
                        icons::entry(
                            p,
                            Rect::from_center_size(pos2(cols[0] + 14.0, cy), vec2(20.0, 20.0)),
                            hit.entry.metadata.kind,
                        );
                        p.with_clip_rect(clip(cols[0], cols[1])).text(
                            pos2(cols[0] + 34.0, cy),
                            Align2::LEFT_CENTER,
                            &hit.entry.name,
                            font.clone(),
                            theme::TEXT,
                        );
                        let loc = hit.path.parent().map(|p| p.to_string()).unwrap_or_default();
                        p.with_clip_rect(clip(cols[1], cols[2])).text(
                            pos2(cols[1] + 6.0, cy),
                            Align2::LEFT_CENTER,
                            loc,
                            font.clone(),
                            dim,
                        );
                        p.with_clip_rect(clip(cols[2], cols[3])).text(
                            pos2(cols[2] + 6.0, cy),
                            Align2::LEFT_CENTER,
                            crate::model::tab_type_label(&hit.entry),
                            font.clone(),
                            dim,
                        );
                        if !hit.entry.metadata.is_dir() {
                            p.text(
                                pos2(cols[4] - 10.0, cy),
                                Align2::RIGHT_CENTER,
                                super::format_size(hit.entry.metadata.size),
                                font,
                                dim,
                            );
                        }
                    }
                    if resp.double_clicked() {
                        actions.push(Action::OpenSearchHit(i));
                    } else if resp.clicked() || resp.secondary_clicked() {
                        clicked = Some(i);
                    }
                    let resp = resp.on_hover_text(hit.path.to_string());
                    resp.context_menu(|ui| {
                        let open = if hit.entry.metadata.is_dir() { "Open" } else { "Preview" };
                        if ui.button(open).clicked() {
                            actions.push(Action::OpenSearchHit(i));
                        }
                        if ui.button("Open file location").clicked() {
                            actions.push(Action::RevealSearchHit(i));
                        }
                    });
                }
                if clicked.is_some() {
                    tab.result_sel = clicked;
                }
            })
        })
        .inner;
    state.list_scroll = (out.state.clamped_offset().y, out.inner_rect.height());
}
