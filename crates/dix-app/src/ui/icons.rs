//! Icons painted with egui shapes, so no icon font or image assets are needed.

use super::theme;
use eframe::egui::{self, pos2, vec2, Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui};

pub fn folder(p: &Painter, r: Rect) {
    let (x, y, w, h) = (r.left(), r.top(), r.width(), r.height());
    let back = Color32::from_rgb(58, 146, 206);
    let front = Color32::from_rgb(96, 186, 240);
    let top = Color32::from_rgb(140, 210, 250);
    p.rect_filled(Rect::from_min_max(pos2(x + w * 0.05, y + h * 0.16), pos2(x + w * 0.45, y + h * 0.4)), 2.0, back);
    p.rect_filled(Rect::from_min_max(pos2(x + w * 0.05, y + h * 0.26), pos2(x + w * 0.95, y + h * 0.86)), 2.0, back);
    p.rect_filled(Rect::from_min_max(pos2(x + w * 0.05, y + h * 0.36), pos2(x + w * 0.95, y + h * 0.86)), 2.0, front);
    p.line_segment([pos2(x + w * 0.07, y + h * 0.37), pos2(x + w * 0.93, y + h * 0.37)], Stroke::new(1.0, top));
}

pub fn file(p: &Painter, r: Rect) {
    let (x, y, w, h) = (r.left(), r.top(), r.width(), r.height());
    let l = x + w * 0.2;
    let rr = x + w * 0.8;
    let t = y + h * 0.08;
    let b = y + h * 0.92;
    let fold = w * 0.2;
    let body = vec![pos2(l, t), pos2(rr - fold, t), pos2(rr, t + fold), pos2(rr, b), pos2(l, b)];
    p.add(Shape::convex_polygon(
        body,
        Color32::from_rgb(232, 232, 232),
        Stroke::new(1.0, Color32::from_rgb(150, 150, 150)),
    ));
    p.add(Shape::convex_polygon(
        vec![pos2(rr - fold, t), pos2(rr, t + fold), pos2(rr - fold, t + fold)],
        Color32::from_rgb(185, 185, 185),
        Stroke::NONE,
    ));
    let line = Stroke::new((h * 0.04).max(1.0), Color32::from_rgb(170, 170, 170));
    for i in 0..3 {
        let ly = y + h * (0.45 + 0.13 * i as f32);
        p.line_segment([pos2(l + w * 0.1, ly), pos2(rr - w * 0.1, ly)], line);
    }
}

pub fn symlink_badge(p: &Painter, r: Rect) {
    let c =
        Rect::from_min_size(pos2(r.left(), r.bottom() - r.height() * 0.45), vec2(r.width() * 0.45, r.height() * 0.45));
    p.rect_filled(c, 2.0, Color32::WHITE);
    let s = Stroke::new(1.5, Color32::from_rgb(30, 90, 160));
    p.line_segment(
        [
            pos2(c.left() + c.width() * 0.25, c.bottom() - c.height() * 0.25),
            pos2(c.right() - c.width() * 0.25, c.top() + c.height() * 0.25),
        ],
        s,
    );
    p.line_segment(
        [
            pos2(c.right() - c.width() * 0.25, c.top() + c.height() * 0.25),
            pos2(c.right() - c.width() * 0.25, c.top() + c.height() * 0.6),
        ],
        s,
    );
    p.line_segment(
        [
            pos2(c.right() - c.width() * 0.25, c.top() + c.height() * 0.25),
            pos2(c.left() + c.width() * 0.6, c.top() + c.height() * 0.25),
        ],
        s,
    );
}

pub fn disk(p: &Painter, r: Rect) {
    let body = r.shrink2(vec2(r.width() * 0.08, r.height() * 0.2));
    p.rect_filled(body, 3.0, Color32::from_rgb(170, 170, 175));
    p.rect_filled(
        Rect::from_min_max(pos2(body.left(), body.center().y), body.max),
        3.0,
        Color32::from_rgb(140, 140, 145),
    );
    p.circle_filled(
        pos2(body.right() - body.width() * 0.18, body.center().y + body.height() * 0.22),
        body.height() * 0.1,
        Color32::from_rgb(80, 220, 120),
    );
}

/// Draws an entry icon for a [`dix_core::EntryKind`].
pub fn entry(p: &Painter, r: Rect, kind: dix_core::EntryKind) {
    match kind {
        dix_core::EntryKind::Directory => folder(p, r),
        dix_core::EntryKind::Symlink => {
            file(p, r);
            symlink_badge(p, r);
        }
        _ => file(p, r),
    }
}

#[derive(Clone, Copy)]
pub enum Glyph {
    Back,
    Forward,
    Up,
    Refresh,
    Close,
    Plus,
    TriangleRight,
    TriangleDown,
    Eject,
}

pub fn glyph(p: &Painter, r: Rect, g: Glyph, color: Color32) {
    let c = r.center();
    let s = r.width().min(r.height()) * 0.32;
    let stroke = Stroke::new(1.8, color);
    let arrow = |from: Pos2, to: Pos2| {
        p.line_segment([from, to], stroke);
        let dir = (to - from).normalized();
        let n = vec2(-dir.y, dir.x);
        let head = s * 0.6;
        p.line_segment([to, to - dir * head + n * head], stroke);
        p.line_segment([to, to - dir * head - n * head], stroke);
    };
    match g {
        Glyph::Back => arrow(c + vec2(s, 0.0), c - vec2(s, 0.0)),
        Glyph::Forward => arrow(c - vec2(s, 0.0), c + vec2(s, 0.0)),
        Glyph::Up => arrow(c + vec2(0.0, s), c - vec2(0.0, s)),
        Glyph::Refresh => {
            let pts: Vec<Pos2> = (0..=20)
                .map(|i| {
                    let a = -0.9 + i as f32 / 20.0 * 5.0;
                    c + vec2(a.cos(), a.sin()) * s
                })
                .collect();
            let end = *pts.last().unwrap();
            p.add(Shape::line(pts, stroke));
            let a: f32 = -0.9 + 5.0;
            let tangent = vec2(-a.sin(), a.cos());
            let radial = vec2(a.cos(), a.sin());
            p.add(Shape::convex_polygon(
                vec![end + tangent * s * 0.55, end + radial * s * 0.4, end - radial * s * 0.4],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Close => {
            let d = s * 0.75;
            p.line_segment([c - vec2(d, d), c + vec2(d, d)], stroke);
            p.line_segment([c + vec2(-d, d), c + vec2(d, -d)], stroke);
        }
        Glyph::Plus => {
            p.line_segment([c - vec2(s, 0.0), c + vec2(s, 0.0)], stroke);
            p.line_segment([c - vec2(0.0, s), c + vec2(0.0, s)], stroke);
        }
        Glyph::TriangleRight => {
            let d = s * 0.7;
            p.add(Shape::convex_polygon(
                vec![c + vec2(-d * 0.6, -d), c + vec2(d * 0.8, 0.0), c + vec2(-d * 0.6, d)],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Eject => {
            // ⏏: a triangle above a bar.
            let w = s * 1.05;
            p.add(Shape::convex_polygon(
                vec![c + vec2(0.0, -s), c + vec2(w, s * 0.25), c + vec2(-w, s * 0.25)],
                color,
                Stroke::NONE,
            ));
            p.rect_filled(Rect::from_min_max(c + vec2(-w, s * 0.55), c + vec2(w, s * 0.95)), 1.0, color);
        }
        Glyph::TriangleDown => {
            let d = s * 0.7;
            p.add(Shape::convex_polygon(
                vec![c + vec2(-d, -d * 0.6), c + vec2(d, -d * 0.6), c + vec2(0.0, d * 0.8)],
                color,
                Stroke::NONE,
            ));
        }
    }
}

/// A square, frameless icon button.
pub fn button(ui: &mut Ui, g: Glyph, enabled: bool, tooltip: &str) -> Response {
    let size = vec2(28.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        if enabled && resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(60, 60, 60));
        }
        let color = if enabled { theme::TEXT } else { Color32::from_gray(90) };
        glyph(ui.painter(), rect, g, color);
    }
    resp.on_hover_text(tooltip)
}

/// Allocates an icon slot of `size` and paints an entry icon into it.
pub fn entry_widget(ui: &mut Ui, kind: dix_core::EntryKind, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::Vec2::splat(size), Sense::hover());
    entry(ui.painter(), rect, kind);
    resp
}
