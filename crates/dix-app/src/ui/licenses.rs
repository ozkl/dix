//! The Licenses window: this app's license (MIT) and those of the software
//! it is built with
//! (THIRD-PARTY-LICENSES.txt, written by scripts/third-party-licenses.sh),
//! embedded so that every build carries them.

use super::theme;
use eframe::egui::{self, RichText};
use std::sync::OnceLock;

const MIT: &str = include_str!("../../../../LICENSE");
const THIRD_PARTY: &str = include_str!("../../../../THIRD-PARTY-LICENSES.txt");

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    ThirdParty,
    Mit,
}

fn lines(page: Page) -> &'static [&'static str] {
    static PAGES: OnceLock<[Vec<&'static str>; 2]> = OnceLock::new();
    let p = PAGES.get_or_init(|| [THIRD_PARTY.lines().collect(), MIT.lines().collect()]);
    match page {
        Page::ThirdParty => &p[0],
        Page::Mit => &p[1],
    }
}

pub fn show(ctx: &egui::Context, open: &mut bool) {
    let id = egui::Id::unique("licenses_page");
    egui::Window::new("Licenses")
        .open(open)
        .default_size([680.0, 560.0])
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.label(format!(
                "{} {} is licensed under the MIT license. It is built with the software listed under \
             Third-party, under their licenses.",
                crate::APP_NAME,
                env!("CARGO_PKG_VERSION")
            ));
            let mut page: Page = ui.data(|d| d.get_temp(id)).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut page, Page::ThirdParty, "Third-party");
                ui.selectable_value(&mut page, Page::Mit, "MIT (this app)");
            });
            ui.data_mut(|d| d.insert_temp(id, page));
            ui.separator();
            // Only the lines in view are laid out (the third-party text is long).
            let text = lines(page);
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let row = ui.fonts_mut(|f| f.row_height(&font));
            // (Rows without spacing: show_rows counts it into each row.)
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::both().id_salt(("licenses", page as u8)).auto_shrink([false, false]).show_rows(
                ui,
                row,
                text.len(),
                |ui, range| {
                    for l in &text[range] {
                        ui.label(RichText::new(*l).monospace().color(theme::TEXT));
                    }
                },
            );
        });
}
