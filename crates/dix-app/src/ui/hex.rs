//! The hex viewer: a file of a volume, or a volume's raw bytes (its whole
//! image or partition), as offset / hex / text rows, in a window of its
//! own (several can be open, the explorer stays usable).
//!
//! Rows are not laid out in a scroll area — a disk of terabytes is too tall
//! for one — but drawn from a top row kept as a number, moved by the
//! wheel, the keys and a scroll bar of its own. Bytes are read in 64 KiB chunks and the
//! last one is kept. A click selects a byte; the inspector shows the
//! integers that start there. *Go to* takes an offset (decimal, `0x` hex)
//! or, for raw views, a sector (`s123`); *Find* takes hex bytes (`4D 5A`)
//! or text in quotes (`"ustar"`) and searches forward from the selection.

use super::theme;
use crate::model::{VolumeId, Workspace};
use dix_core::{BlockDevice, VfsPath};
use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Sense};

const ROW: u64 = 16;
const CHUNK: u64 = 64 << 10;
/// How far one click of *Find next* searches.
const FIND_STEP: u64 = 64 << 20;

pub enum Source {
    File { volume: VolumeId, path: VfsPath },
    Device { dev: Box<dyn BlockDevice>, sector: u64 },
}

pub struct HexView {
    id: u64,
    pub title: String,
    source: Source,
    size: u64,
    top: u64,
    selected: Option<u64>,
    cache: Option<(u64, Vec<u8>)>,
    goto: String,
    find: String,
    /// Where an unfinished search goes on from.
    find_from: Option<u64>,
    message: Option<String>,
    error: Option<String>,
}

impl HexView {
    pub fn file(ws: &Workspace, volume: VolumeId, path: VfsPath) -> Result<HexView, String> {
        let v = ws.volumes.get(&volume).ok_or("the image is no longer open")?;
        let size = v.fs().metadata(&path).map_err(|e| e.to_string())?.size;
        let title = format!("{} — {}", path.file_name().unwrap_or("/"), v.name);
        Ok(HexView::new(title, Source::File { volume, path }, size))
    }

    pub fn device(title: String, dev: Box<dyn BlockDevice>) -> HexView {
        let size = dev.size();
        let sector = dev.info().sector_size.max(1) as u64;
        HexView::new(title, Source::Device { dev, sector }, size)
    }

    fn new(title: String, source: Source, size: u64) -> HexView {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        HexView {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            title,
            source,
            size,
            top: 0,
            selected: None,
            cache: None,
            goto: String::new(),
            find: String::new(),
            find_from: None,
            message: None,
            error: None,
        }
    }

    /// Reads `len` bytes at `at` from the source (fewer at the end).
    fn read_raw(&mut self, ws: &Workspace, at: u64, len: usize) -> Result<Vec<u8>, String> {
        let len = (self.size.saturating_sub(at)).min(len as u64) as usize;
        let mut buf = vec![0u8; len];
        match &mut self.source {
            Source::File { volume, path } => {
                let v = ws.volumes.get(volume).ok_or("the image is no longer open")?;
                let mut fs = v.fs();
                let mut got = 0;
                while got < len {
                    let n = fs.read(path, at + got as u64, &mut buf[got..]).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                buf.truncate(got);
            }
            Source::Device { dev, .. } => dev.read_at(at, &mut buf).map_err(|e| e.to_string())?,
        }
        Ok(buf)
    }

    /// The bytes of [at, at + len), through the chunk cache.
    fn bytes(&mut self, ws: &Workspace, at: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let end = (at + len as u64).min(self.size);
        let mut pos = at;
        while pos < end {
            let chunk = pos - pos % CHUNK;
            if self.cache.as_ref().is_none_or(|(c, _)| *c != chunk) {
                match self.read_raw(ws, chunk, CHUNK as usize) {
                    Ok(b) => {
                        self.cache = Some((chunk, b));
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e);
                        return out;
                    }
                }
            }
            let (_, b) = self.cache.as_ref().unwrap();
            let from = (pos - chunk) as usize;
            if from >= b.len() {
                break;
            }
            let n = ((end - pos) as usize).min(b.len() - from);
            out.extend_from_slice(&b[from..from + n]);
            pos += n as u64;
        }
        out
    }

    fn rows(&self) -> u64 {
        self.size.div_ceil(ROW).max(1)
    }

    fn sector_size(&self) -> Option<u64> {
        match &self.source {
            Source::Device { sector, .. } => Some(*sector),
            Source::File { .. } => None,
        }
    }

    /// Shows `offset`: selects it and scrolls so its row is a few rows down.
    fn go(&mut self, offset: u64, visible: u64) {
        let offset = offset.min(self.size.saturating_sub(1));
        self.selected = Some(offset);
        let row = offset / ROW;
        if row < self.top || row >= self.top + visible.max(1) {
            self.top = row.saturating_sub(visible / 4);
        }
    }

    fn parse_goto(&self, s: &str) -> Option<u64> {
        let s = s.trim().replace(['_', ' '], "");
        let (s, sector) = match s.strip_prefix(['s', 'S']) {
            Some(rest) => (rest.to_owned(), true),
            None => (s, false),
        };
        let n = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(h) => u64::from_str_radix(h, 16).ok()?,
            None => s.parse().ok()?,
        };
        if sector {
            n.checked_mul(self.sector_size().unwrap_or(512))
        } else {
            Some(n)
        }
    }

    /// Searches for the pattern from `from`, at most FIND_STEP bytes.
    fn find_next(&mut self, ws: &Workspace, from: u64, visible: u64) {
        let Some(pat) = parse_pattern(&self.find) else {
            self.message = Some("Find: hex bytes (4D 5A) or text in quotes (\"text\")".into());
            return;
        };
        let stop = (from + FIND_STEP).min(self.size);
        let mut pos = from;
        // Chunks overlap by the pattern's length, so nothing is missed at
        // a boundary.
        while pos < stop {
            let len = ((stop - pos) as usize).min(4 << 20) + pat.len() - 1;
            let data = match self.read_raw(ws, pos, len) {
                Ok(d) => d,
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            };
            if let Some(i) = data.windows(pat.len()).position(|w| w == pat.as_slice()) {
                self.go(pos + i as u64, visible);
                self.find_from = None;
                self.message = Some(format!("Found at {}", offset_text(pos + i as u64)));
                return;
            }
            pos += (len - (pat.len() - 1)) as u64;
        }
        if stop >= self.size {
            self.find_from = None;
            self.message = Some("Not found before the end".into());
        } else {
            self.find_from = Some(stop);
            self.message = Some(format!("Not found up to {}: Find next goes on", offset_text(stop)));
        }
    }
}

/// `4D 5A 90` or `"text"`.
fn parse_pattern(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let t = &s[1..s.len() - 1];
        return (!t.is_empty()).then(|| t.as_bytes().to_vec());
    }
    let hex: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

fn offset_text(o: u64) -> String {
    format!("0x{o:X} ({o})")
}

/// Shows every open hex view; closed ones are removed.
pub fn show(ctx: &egui::Context, ws: &Workspace, views: &mut Vec<HexView>) {
    views.retain_mut(|v| {
        let mut open = true;
        egui::Window::new(&v.title)
            .id(egui::Id::unique(("hex", v.id)))
            .open(&mut open)
            .default_size([760.0, 520.0])
            .min_width(640.0)
            .resizable(true)
            .show(ctx, |ui| view(ui, ws, v));
        open
    });
}

fn view(ui: &mut egui::Ui, ws: &Workspace, v: &mut HexView) {
    let font = FontId::monospace(13.0);
    let probe = ui.painter().layout_no_wrap("0".into(), font.clone(), theme::TEXT);
    let row_h = probe.size().y + 2.0;
    let char_w = probe.size().x;

    // The tools.
    let mut visible = 32u64;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} bytes", v.size)).color(theme::TEXT_DIM));
        if let Some(s) = v.sector_size() {
            ui.label(RichText::new(format!("· {} sectors of {s}", v.size.div_ceil(s))).color(theme::TEXT_DIM));
        }
    });
    ui.horizontal(|ui| {
        ui.label("Go to:");
        let hint = if v.sector_size().is_some() { "offset, 0x…, or s<sector>" } else { "offset or 0x…" };
        let r = ui.add(egui::TextEdit::singleline(&mut v.goto).hint_text(hint).desired_width(170.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Go").clicked() || enter {
            match v.parse_goto(&v.goto.clone()) {
                Some(o) if o < v.size => {
                    v.go(o, visible);
                    v.message = None;
                }
                _ => v.message = Some("Go to: an offset inside the data".into()),
            }
        }
        ui.separator();
        ui.label("Find:");
        let r = ui.add(egui::TextEdit::singleline(&mut v.find).hint_text("4D 5A or \"text\"").desired_width(170.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if r.changed() {
            v.find_from = None;
        }
        if ui.button("Find next").clicked() || enter {
            let from = v.find_from.unwrap_or_else(|| v.selected.map_or(0, |s| s + 1));
            v.find_next(ws, from, visible);
        }
    });
    if let Some(m) = &v.message {
        ui.label(RichText::new(m).color(theme::TEXT_DIM));
    }
    if let Some(e) = &v.error {
        ui.colored_label(theme::DANGER, format!("Cannot read: {e}"));
    }
    ui.separator();

    // The inspector, at the bottom.
    let inspector_h = 4.0 * row_h + 12.0;
    let avail = ui.available_size();
    let list_h = (avail.y - inspector_h).max(row_h * 4.0);
    visible = ((list_h / row_h).floor() as u64).max(1);
    let rows = v.rows();
    let max_top = rows.saturating_sub(visible);

    {
        // One rectangle for the rows and the scroll bar: nothing in it can
        // make the window wider.
        let w = ui.available_width().max(200.0);
        let (whole, _) = ui.allocate_exact_size(egui::vec2(w, list_h), Sense::hover());
        let bar = egui::Rect::from_min_max(egui::pos2(whole.right() - 18.0, whole.top()), whole.max);
        let list = egui::Rect::from_min_max(whole.min, egui::pos2(bar.left() - 6.0, whole.bottom()));
        let resp = ui.interact(list, egui::Id::unique(("hex-rows", v.id)), Sense::click());
        let rect = list;
        // Scrolling: wheel, keys.
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                let rows_moved = (dy / row_h).round() as i64;
                v.top = (v.top as i64 - rows_moved).clamp(0, max_top as i64) as u64;
            }
        }
        if resp.hovered() || resp.has_focus() {
            ui.input(|i| {
                if i.key_pressed(egui::Key::PageDown) {
                    v.top = (v.top + visible).min(max_top);
                }
                if i.key_pressed(egui::Key::PageUp) {
                    v.top = v.top.saturating_sub(visible);
                }
                if i.key_pressed(egui::Key::Home) && i.modifiers.command {
                    v.top = 0;
                }
                if i.key_pressed(egui::Key::End) && i.modifiers.command {
                    v.top = max_top;
                }
                if i.key_pressed(egui::Key::ArrowDown) {
                    v.top = (v.top + 1).min(max_top);
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    v.top = v.top.saturating_sub(1);
                }
            });
        }
        v.top = v.top.min(max_top);
        let start = v.top * ROW;
        let data = v.bytes(ws, start, (visible * ROW) as usize);
        // Columns: offset (12), hex (16 × 3 + 1), text (16).
        let hex_x = 12.0 + 2.0;
        let text_x = hex_x + 16.0 * 3.0 + 2.0;
        if let (true, Some(p)) = (resp.clicked(), resp.interact_pointer_pos()) {
            let col = ((p.x - rect.left()) / char_w) as f32;
            let row = ((p.y - rect.top()) / row_h) as u64;
            let byte = if col >= text_x && col < text_x + 16.0 {
                Some((col - text_x) as u64)
            } else if col >= hex_x && col < hex_x + 16.0 * 3.0 + 1.0 {
                let c = col - hex_x;
                let c = if c >= 8.0 * 3.0 { c - 1.0 } else { c };
                Some(((c / 3.0) as u64).min(15))
            } else {
                None
            };
            if let Some(b) = byte {
                let o = start + row * ROW + b;
                if o < v.size {
                    v.selected = Some(o);
                    v.find_from = None;
                }
            }
        }
        let painter = ui.painter_at(rect);
        let fmt = |color: Color32, bg: Color32| TextFormat {
            font_id: font.clone(),
            color,
            background: bg,
            ..Default::default()
        };
        let none = Color32::TRANSPARENT;
        let sel_bg = theme::ACCENT.gamma_multiply(0.55);
        for r in 0..visible {
            let off = start + r * ROW;
            if off >= v.size {
                break;
            }
            let at = (r * ROW) as usize;
            let row = &data[at.min(data.len())..(at + ROW as usize).min(data.len())];
            let mut job = LayoutJob::default();
            let sector_start = v.sector_size().is_some_and(|s| off.is_multiple_of(s));
            let off_color = if sector_start { theme::ACCENT } else { theme::TEXT_DIM };
            job.append(&format!("{off:012X}  "), 0.0, fmt(off_color, none));
            for i in 0..16usize {
                let sel = v.selected == Some(off + i as u64);
                let bg = if sel { sel_bg } else { none };
                let gap = if i == 7 { "  " } else { " " };
                match row.get(i) {
                    Some(b) => {
                        let color = if *b == 0 { theme::TEXT_DIM } else { theme::TEXT };
                        job.append(&format!("{b:02X}"), 0.0, fmt(color, bg));
                    }
                    None => job.append("  ", 0.0, fmt(theme::TEXT, none)),
                }
                job.append(gap, 0.0, fmt(theme::TEXT, none));
            }
            job.append(" ", 0.0, fmt(theme::TEXT, none));
            for i in 0..16usize {
                let sel = v.selected == Some(off + i as u64);
                let bg = if sel { sel_bg } else { none };
                match row.get(i) {
                    Some(&b) if (0x20..0x7F).contains(&b) => {
                        job.append(&(b as char).to_string(), 0.0, fmt(theme::TEXT, bg))
                    }
                    Some(_) => job.append("·", 0.0, fmt(theme::TEXT_DIM, bg)),
                    None => job.append(" ", 0.0, fmt(theme::TEXT, none)),
                }
            }
            let galley = painter.layout_job(job);
            painter.galley(egui::pos2(rect.left(), rect.top() + r as f32 * row_h), galley, theme::TEXT);
        }
        // The scroll bar: a thumb on a track, dragged or clicked.
        let track = ui.interact(bar, egui::Id::unique(("hex-bar", v.id)), Sense::click_and_drag());
        let thumb_h = (bar.height() * visible as f32 / rows as f32).clamp(24.0, bar.height());
        let room = (bar.height() - thumb_h).max(1.0);
        if let (true, Some(p)) = (track.dragged() || track.clicked(), track.interact_pointer_pos()) {
            let f = ((p.y - bar.top() - thumb_h / 2.0) / room).clamp(0.0, 1.0) as f64;
            v.top = (f * max_top as f64).round() as u64;
        }
        let f = if max_top == 0 { 0.0 } else { v.top as f32 / max_top as f32 };
        let thumb = egui::Rect::from_min_size(
            egui::pos2(bar.left() + 3.0, bar.top() + f * room),
            egui::vec2(bar.width() - 6.0, thumb_h),
        );
        let p = ui.painter();
        p.rect_filled(bar, 4.0, theme::FIELD_BG);
        let active = track.dragged() || track.hovered();
        p.rect_filled(thumb, 4.0, if active { theme::ACCENT_HOVER } else { theme::BUTTON_BG });
    }

    // The inspector.
    ui.separator();
    match v.selected {
        Some(o) => {
            let b = v.bytes(ws, o, 8);
            let int = |n: usize, le: bool| -> Option<u64> {
                let s = b.get(..n)?;
                let mut x = 0u64;
                for (i, &c) in s.iter().enumerate() {
                    let shift = if le { i } else { n - 1 - i } * 8;
                    x |= (c as u64) << shift;
                }
                Some(x)
            };
            let show = |n: usize, le: bool| int(n, le).map_or("—".to_owned(), |x| x.to_string());
            let mut head = format!("Offset {}", offset_text(o));
            if let Some(s) = v.sector_size() {
                head += &format!(" · sector {} + {}", o / s, o % s);
            }
            ui.label(RichText::new(head).strong());
            let u8v = b.first().map_or("—".to_owned(), |x| format!("{x} (signed {})", *x as i8));
            let mono = |t: String| RichText::new(t).font(FontId::monospace(12.0));
            ui.label(mono(format!(
                "u8 {u8v}   u16 LE {} BE {}   u32 LE {} BE {}",
                show(2, true),
                show(2, false),
                show(4, true),
                show(4, false)
            )));
            ui.label(mono(format!("u64 LE {} BE {}", show(8, true), show(8, false))));
        }
        None => {
            ui.label(RichText::new("Click a byte to inspect it.").color(theme::TEXT_DIM));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_and_offsets() {
        assert_eq!(parse_pattern("4D 5a"), Some(vec![0x4D, 0x5A]));
        assert_eq!(parse_pattern("\"ustar\""), Some(b"ustar".to_vec()));
        assert_eq!(parse_pattern("4"), None);
        assert_eq!(parse_pattern("zz"), None);
        let dev = dix_core::device::MemoryDevice::new("m", vec![0u8; 4096]);
        let v = HexView::device("t".into(), Box::new(dev));
        assert_eq!(v.parse_goto("0x10"), Some(16));
        assert_eq!(v.parse_goto("1_000"), Some(1000));
        assert_eq!(v.parse_goto("s2"), Some(1024));
        assert_eq!(v.parse_goto("nope"), None);
    }

    #[test]
    fn finds_across_chunks() {
        let mut data = vec![0u8; 9 << 20];
        data[(4 << 20) - 2..(4 << 20) + 3].copy_from_slice(b"HELLO");
        let dev = dix_core::device::MemoryDevice::new("m", data);
        let mut v = HexView::device("t".into(), Box::new(dev));
        let ws = Workspace::new(dix_drivers::default_registry());
        v.find = "\"HELLO\"".into();
        v.find_next(&ws, 0, 10);
        assert_eq!(v.selected, Some((4 << 20) - 2));
        v.find_next(&ws, v.selected.unwrap() + 1, 10);
        assert_eq!(v.selected, Some((4 << 20) - 2), "no second match");
        assert!(v.message.as_deref().unwrap().starts_with("Not found"));
    }
}
