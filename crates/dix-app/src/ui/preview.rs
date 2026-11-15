//! Preview pane: metadata of the selected entry and a text / hex preview.

use super::{icons, theme};
use crate::model::{VolumeId, Workspace};
use dix_core::{EntryKind, Metadata, VfsPath};
use eframe::egui::{self, RichText, Ui};

const TEXT_LIMIT: usize = 64 * 1024;
const HEX_LIMIT: usize = 4 * 1024;

#[derive(Default)]
pub enum Content {
    #[default]
    Nothing,
    Text {
        text: String,
        truncated: bool,
    },
    Hex {
        dump: String,
        truncated: bool,
    },
    Folder {
        files: usize,
        dirs: usize,
    },
    Error(String),
}

#[derive(Default)]
pub struct PreviewCache {
    key: Option<(VolumeId, VfsPath, u64, Option<i64>)>,
    target: Option<(String, Metadata)>,
    content: Content,
}

/// Refreshes the cached preview if the selection changed.
pub fn update(cache: &mut PreviewCache, ws: &mut Workspace) {
    let Some((tab, vol)) = ws.current() else {
        *cache = PreviewCache::default();
        return;
    };
    let Some((path, entry)) = tab.preview_target() else {
        cache.key = None;
        cache.target = None;
        cache.content = Content::Nothing;
        return;
    };
    let key = (vol.id, path.clone(), entry.metadata.size, entry.metadata.modified);
    if cache.key.as_ref() == Some(&key) {
        return;
    }
    cache.content = match entry.metadata.kind {
        EntryKind::Directory => match vol.fs().read_dir(&path) {
            Ok(list) => {
                let dirs = list.iter().filter(|e| e.metadata.is_dir()).count();
                Content::Folder { files: list.len() - dirs, dirs }
            }
            Err(e) => Content::Error(e.to_string()),
        },
        EntryKind::File => {
            let mut buf = vec![0u8; TEXT_LIMIT.min(entry.metadata.size as usize)];
            match vol.fs().read(&path, 0, &mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    classify(buf, entry.metadata.size)
                }
                Err(e) => Content::Error(e.to_string()),
            }
        }
        EntryKind::Symlink => Content::Text {
            text: format!("→ {}", entry.metadata.symlink_target.clone().unwrap_or_default()),
            truncated: false,
        },
        EntryKind::Other => Content::Nothing,
    };
    cache.key = Some(key);
    cache.target = Some((entry.name, entry.metadata));
}

fn classify(buf: Vec<u8>, total: u64) -> Content {
    let truncated_text = (buf.len() as u64) < total;
    let text_ok = !buf.contains(&0)
        && match std::str::from_utf8(&buf) {
            Ok(_) => true,
            // A multi-byte character cut at the end of the window is fine.
            Err(e) => e.error_len().is_none() && truncated_text,
        };
    if text_ok && !buf.is_empty() {
        let valid = std::str::from_utf8(&buf).map(|s| s.len()).unwrap_or_else(|e| e.valid_up_to());
        return Content::Text { text: String::from_utf8_lossy(&buf[..valid]).into_owned(), truncated: truncated_text };
    }
    let n = buf.len().min(HEX_LIMIT);
    Content::Hex { dump: hex_dump(&buf[..n]), truncated: (n as u64) < total }
}

pub fn hex_dump(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4);
    for (i, chunk) in data.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i * 16));
        for j in 0..16 {
            match chunk.get(j) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        out.extend(chunk.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }));
        out.push('\n');
    }
    out
}

pub fn show(ui: &mut Ui, cache: &PreviewCache) {
    let Some((name, meta)) = &cache.target else {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Select a single item to preview it.").color(theme::TEXT_DIM))
        });
        return;
    };
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        icons::entry_widget(ui, meta.kind, 40.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(name).size(16.0).strong());
            let size = if meta.is_dir() { String::new() } else { super::format_size(meta.size) };
            ui.label(RichText::new(size).color(theme::TEXT_DIM));
        });
    });
    ui.add_space(4.0);
    egui::Grid::new("preview_meta").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        let mut row = |k: &str, v: String| {
            if !v.is_empty() {
                ui.label(RichText::new(k).color(theme::TEXT_DIM));
                ui.label(v);
                ui.end_row();
            }
        };
        row("Modified", super::format_time(meta.modified));
        row("Created", super::format_time(meta.created));
        row("Permissions", meta.mode.map(super::format_mode).unwrap_or_default());
        if let (Some(u), Some(g)) = (meta.uid, meta.gid) {
            row("Owner", format!("{u}:{g}"));
        }
    });
    ui.separator();
    match &cache.content {
        Content::Nothing => {}
        Content::Folder { files, dirs } => {
            ui.label(format!("{dirs} folder(s), {files} file(s)"));
        }
        Content::Error(e) => {
            ui.colored_label(theme::DANGER, e);
        }
        Content::Text { text, truncated } => scroll_text(ui, text, *truncated, "first 64 KB shown", true),
        Content::Hex { dump, truncated } => scroll_text(ui, dump, *truncated, "first 4 KB shown", false),
    }
}

fn scroll_text(ui: &mut Ui, text: &str, truncated: bool, note: &str, wrap: bool) {
    if truncated {
        ui.label(RichText::new(note).small().color(theme::TEXT_DIM));
    }
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        ui.add(
            egui::Label::new(RichText::new(text).monospace().size(12.0))
                .wrap_mode(if wrap { egui::TextWrapMode::Wrap } else { egui::TextWrapMode::Extend })
                .selectable(true),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_text_and_binary() {
        assert!(matches!(classify(b"hello".to_vec(), 5), Content::Text { .. }));
        assert!(matches!(classify(vec![0, 1, 2], 3), Content::Hex { .. }));
        assert!(hex_dump(b"AB").starts_with("00000000  41 42"));
    }
}
