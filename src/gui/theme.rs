//! Light, card-based look: grey window background, white rounded cards,
//! pill-style tabs, value tiles with gauges and icon badges.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, Sense, Shadow, Stroke,
    StrokeKind, TextStyle, Vec2,
};

pub const BG: Color32 = Color32::from_rgb(241, 243, 246);
pub const CARD: Color32 = Color32::WHITE;
pub const FIELD: Color32 = Color32::from_rgb(244, 246, 249);
pub const BORDER: Color32 = Color32::from_rgb(225, 229, 235);
pub const TEXT: Color32 = Color32::from_rgb(31, 35, 40);
pub const WEAK: Color32 = Color32::from_rgb(107, 114, 128);
pub const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
const TRACK: Color32 = Color32::from_rgb(230, 233, 238);

/// Vertical gap between cards.
pub const GAP: f32 = 10.0;
/// Page width from which tabs lay their cards out in two columns.
pub const WIDE_MIN: f32 = 900.0;
const CARD_MARGIN: i8 = 14;

/// Applies the theme to every style egui keeps (light and dark).
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        use egui::FontFamily::{Monospace, Proportional};
        style.text_styles = [
            (TextStyle::Small, FontId::new(12.0, Proportional)),
            (TextStyle::Body, FontId::new(14.5, Proportional)),
            (TextStyle::Button, FontId::new(14.5, Proportional)),
            (TextStyle::Heading, FontId::new(19.0, Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, Monospace)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 7.0);
        s.button_padding = Vec2::new(14.0, 6.0);
        s.interact_size.y = 30.0;
        s.window_margin = Margin::same(18);

        let mut v = egui::Visuals::light();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = FIELD;
        v.window_corner_radius = CornerRadius::same(12);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_shadow = Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(40),
        };
        v.popup_shadow = Shadow {
            offset: [0, 3],
            blur: 10,
            spread: 0,
            color: Color32::from_black_alpha(30),
        };
        v.selection.bg_fill = ACCENT.gamma_multiply(0.2);
        v.selection.stroke = Stroke::new(1.5, ACCENT);
        v.hyperlink_color = ACCENT;

        let radius = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.corner_radius = radius;
        for (wv, fill, border) in [
            (&mut w.inactive, Color32::from_rgb(236, 239, 243), BORDER),
            (
                &mut w.hovered,
                Color32::from_rgb(226, 231, 238),
                Color32::from_rgb(203, 210, 220),
            ),
            (
                &mut w.active,
                Color32::from_rgb(214, 221, 231),
                Color32::from_rgb(190, 199, 211),
            ),
            (&mut w.open, Color32::from_rgb(226, 231, 238), BORDER),
        ] {
            wv.bg_fill = fill;
            wv.weak_bg_fill = fill;
            wv.bg_stroke = Stroke::new(1.0, border);
            wv.fg_stroke = Stroke::new(1.0, TEXT);
            wv.corner_radius = radius;
            wv.expansion = 0.0;
        }
        style.visuals = v;
    });
}

/// Margins around the content of the central area.
pub fn page() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin {
        left: 16,
        right: 16,
        top: 6,
        bottom: 4,
    })
}

/// A white card with an optional small caption at the top.
pub fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .shadow(Shadow {
            offset: [0, 1],
            blur: 4,
            spread: 0,
            color: Color32::from_black_alpha(12),
        })
        .inner_margin(CARD_MARGIN)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                caption(ui, title);
            }
            add(ui)
        })
        .inner;
    // GAP in total, counting the spacing egui adds after every widget.
    ui.add_space((GAP - ui.spacing().item_spacing.y).max(0.0));
    r
}

pub fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(12.5).strong().color(WEAK));
}

/// Pill-style tab strip. Drawn by hand so every tab keeps its size on hover.
pub fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, items: &[(T, &str)]) {
    Frame::new()
        .fill(TRACK)
        .corner_radius(10)
        .inner_margin(3)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (item, label) in items {
                    let selected = *value == *item;
                    let galley = ui.painter().layout_no_wrap(
                        label.to_string(),
                        FontId::proportional(14.5),
                        TEXT,
                    );
                    let size = Vec2::new(galley.size().x + 30.0, 30.0);
                    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
                    let p = ui.painter();
                    if selected {
                        p.rect(
                            rect,
                            8.0,
                            CARD,
                            Stroke::new(1.0, BORDER),
                            StrokeKind::Inside,
                        );
                    } else if response.hovered() {
                        p.rect_filled(rect, 8.0, Color32::from_white_alpha(150));
                    }
                    let color = if selected || response.hovered() {
                        TEXT
                    } else {
                        WEAK
                    };
                    p.galley_with_override_text_color(
                        rect.center() - galley.size() / 2.0,
                        galley,
                        color,
                    );
                    if response.clicked() {
                        *value = *item;
                    }
                }
            });
        });
}

/// A filled button for the main action of a card.
pub fn primary(text: &str, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong())
        .fill(color)
        .stroke(Stroke::NONE)
}

/// A round tinted badge with an icon glyph (or short text) in the middle.
pub fn badge(p: &egui::Painter, center: egui::Pos2, radius: f32, glyph: &str, color: Color32) {
    p.circle_filled(center, radius, color.gamma_multiply(0.14));
    let size = if glyph.chars().count() > 1 {
        radius * 0.75
    } else {
        radius
    };
    p.text(
        center,
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(size),
        color,
    );
}

/// A value tile: icon badge, caption, big value, a gauge and a note under it.
pub fn tile(
    ui: &mut egui::Ui,
    icon: &str,
    caption_text: &str,
    value: &str,
    gauge: (f32, Color32),
    note: &str,
) {
    let (fraction, color) = gauge;
    card(ui, "", |ui| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
            badge(ui.painter(), r.center(), 15.0, icon, color);
            caption(ui, caption_text);
        });
        ui.label(RichText::new(value).size(23.0).strong().color(TEXT));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
        ui.painter().rect_filled(rect, 3.0, TRACK);
        let mut filled = rect;
        filled.set_width(rect.width() * fraction.clamp(0.0, 1.0));
        ui.painter().rect_filled(filled, 3.0, color);
        ui.label(RichText::new(note).size(12.5).color(WEAK));
    });
}

/// A number field with − and + buttons. Unlike `DragValue` it does not
/// change the mouse cursor or react to dragging. Returns true on change.
#[allow(clippy::too_many_arguments)]
pub fn stepper(
    ui: &mut egui::Ui,
    id_salt: &str,
    value: &mut f64,
    min: f64,
    max: f64,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> bool {
    let id = egui::Id::new(("stepper", id_salt));
    let scale = 10f64.powi(decimals as i32);
    let set = |value: &mut f64, v: f64| *value = (v.clamp(min, max) * scale).round() / scale;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let button = Vec2::new(30.0, 30.0);
        if ui
            .add_enabled(*value > min, egui::Button::new("−").min_size(button))
            .clicked()
        {
            set(value, *value - step);
            changed = true;
        }
        let shown = format!("{:.*}", decimals, *value);
        let mut text = ui.data(|d| d.get_temp::<String>(id)).unwrap_or(shown);
        let field = egui::TextEdit::singleline(&mut text)
            .desired_width(62.0)
            .horizontal_align(egui::Align::Center)
            .margin(Margin::symmetric(6, 6));
        let r = ui.add(field);
        if r.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, text));
        } else {
            if r.lost_focus() {
                if let Ok(v) = text.trim().replace(',', ".").parse::<f64>() {
                    set(value, v);
                    changed = true;
                }
            }
            ui.data_mut(|d| d.remove::<String>(id));
        }
        if ui
            .add_enabled(*value < max, egui::Button::new("+").min_size(button))
            .clicked()
        {
            set(value, *value + step);
            changed = true;
        }
        ui.label(suffix);
    });
    changed
}

/// A status lamp with its label.
pub fn lamp(ui: &mut egui::Ui, label: &str, on: bool, color: Color32) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        if on {
            ui.painter()
                .circle_filled(r.center(), 8.5, color.gamma_multiply(0.22));
            ui.painter().circle_filled(r.center(), 5.0, color);
        } else {
            ui.painter().circle(
                r.center(),
                5.0,
                FIELD,
                Stroke::new(1.5, Color32::from_rgb(200, 206, 214)),
            );
        }
        ui.label(RichText::new(label).color(if on { TEXT } else { WEAK }));
    });
}

/// The coloured status banner at the top of the window.
pub fn banner(ui: &mut egui::Ui, color: Color32, title: &str, detail: &str) {
    Frame::new()
        .fill(color)
        .corner_radius(12)
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                let p = ui.painter();
                p.circle_filled(r.center(), 20.0, Color32::from_white_alpha(50));
                bolt(p, r.center(), 12.5);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(
                        RichText::new(title)
                            .size(20.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    if !detail.is_empty() {
                        ui.label(
                            RichText::new(detail)
                                .size(13.5)
                                .color(Color32::from_white_alpha(230)),
                        );
                    }
                });
            });
        });
}

/// A white lightning bolt `h` half-height around `c`.
fn bolt(p: &egui::Painter, c: egui::Pos2, h: f32) {
    let pt = |x: f32, y: f32| c + Vec2::new(x * h, y * h);
    let fill = Color32::WHITE;
    // Two triangles meeting at the waist.
    p.add(egui::Shape::convex_polygon(
        vec![pt(0.2, -1.0), pt(-0.55, 0.12), pt(0.05, 0.12)],
        fill,
        Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        vec![pt(-0.05, -0.12), pt(0.55, -0.12), pt(-0.2, 1.0)],
        fill,
        Stroke::NONE,
    ));
}
