//! Widgets drawn to match the C# EBS (WPF) look: frameless icon buttons, toggle icons,
//! the WPF checkbox, the fader, the gradient peak meter, the record button and list rows.

use crate::app::{ACCENT, LINE, MUTED, REC, SELECT, TEXT};
use eframe::egui::{self, pos2, vec2, Color32, FontFamily, FontId, Rect, Response, Sense, Stroke, Ui, WidgetInfo, WidgetType};

pub const FIELD: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
pub const HOVER: Color32 = Color32::from_rgb(0x30, 0x34, 0x3A);
pub const PRESS: Color32 = Color32::from_rgb(0x3A, 0x3F, 0x46);
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x26, 0x2A, 0x30);
pub const TRACK: Color32 = Color32::from_rgb(0x3A, 0x3E, 0x45);
pub const REC_BG: Color32 = Color32::from_rgb(0x3A, 0x1E, 0x21);
pub const FIELD_HOVER_LINE: Color32 = Color32::from_rgb(0x4A, 0x4F, 0x57);
const DISABLED_ALPHA: f32 = 0.4;

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn fade(c: Color32, enabled: bool) -> Color32 {
    if enabled {
        c
    } else {
        c.gamma_multiply(DISABLED_ALPHA)
    }
}

/// Fill for a WPF-style control in its current interaction state.
fn state_fill(r: &Response, normal: Color32) -> Color32 {
    if !r.enabled() {
        normal
    } else if r.is_pointer_button_down_on() {
        PRESS
    } else if r.hovered() {
        HOVER
    } else {
        normal
    }
}

/// Section header ("SCENES", "SOURCES", ...): 11 px semibold, muted.
pub fn header(ui: &mut Ui, t: &str) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(t).font(semibold(11.0)).color(MUTED));
    });
    ui.add_space(2.0);
}

/// Thin vertical divider (top bar), like the C# 1 px border with 14,8 margin.
pub fn vsep(ui: &mut Ui) {
    let h = ui.available_height();
    let (r, _) = ui.allocate_exact_size(vec2(29.0, h), Sense::hover());
    ui.painter().vline(r.center().x, (r.min.y + 8.0)..=(r.max.y - 8.0), Stroke::new(1.0, LINE));
}

/// Frameless 26x24 icon button (C# "IconButton"): transparent, grey fill on hover.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, enabled: bool) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(26.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    let r = if enabled { r } else { r.on_disabled_hover_text(tip) };
    if ui.is_rect_visible(rect) {
        let fill = state_fill(&r, Color32::TRANSPARENT);
        ui.painter().rect_filled(rect, 3.0, fill);
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, icon_font(12.0), fade(TEXT, enabled));
    }
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, tip));
    r.on_hover_text(tip)
}

/// 22x22 toggle icon (source eye / lock, mixer mute): muted when off, light when on.
pub fn toggle_icon(ui: &mut Ui, glyph: &str, on: bool, color: Option<Color32>, tip: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if r.hovered() {
            ui.painter().rect_filled(rect, 3.0, HOVER);
        }
        let c = color.unwrap_or(if on { TEXT } else { MUTED });
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, icon_font(12.0), c);
    }
    r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, tip));
    r.on_hover_text(tip)
}

/// Text for a button with an icon then text, 6 px apart (icon 13 px, text 12 px).
pub fn icon_label(glyph: &str, text: &str, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(glyph, 0.0, egui::TextFormat { font_id: icon_font(13.0), color, valign: egui::Align::Center, ..Default::default() });
    job.append(text, 6.0, egui::TextFormat { font_id: FontId::proportional(12.0), color, valign: egui::Align::Center, ..Default::default() });
    job
}

/// Text then a small chevron (collection button).
pub fn label_chevron(text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(text, 0.0, egui::TextFormat { font_id: FontId::proportional(12.0), color: TEXT, valign: egui::Align::Center, ..Default::default() });
    job.append("\u{E70D}", 6.0, egui::TextFormat { font_id: icon_font(10.0), color: TEXT, valign: egui::Align::Center, ..Default::default() });
    job
}

/// WPF default checkbox: 13 px white box with a dark border and dark tick.
pub fn checkbox(ui: &mut Ui, value: &mut bool, text: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.0), TEXT);
    let size = vec2(13.0 + 5.0 + galley.size().x, 22.0f32.max(galley.size().y));
    let (rect, mut r) = ui.allocate_exact_size(size, Sense::click());
    if r.clicked() {
        *value = !*value;
        r.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let b = Rect::from_min_size(pos2(rect.min.x, rect.center().y - 6.5), vec2(13.0, 13.0));
        let (fill, line, tick) = if r.hovered() {
            (Color32::from_rgb(0xF3, 0xF9, 0xFF), Color32::from_rgb(0x00, 0x54, 0x99), Color32::from_rgb(0x00, 0x54, 0x99))
        } else {
            (Color32::WHITE, Color32::from_rgb(0x33, 0x33, 0x33), Color32::from_rgb(0x21, 0x21, 0x21))
        };
        let p = ui.painter();
        p.rect_filled(b, 0.0, fill);
        p.rect_stroke(b.shrink(0.5), 0.0, Stroke::new(1.0, line), egui::StrokeKind::Middle);
        if *value {
            let pts = vec![pos2(b.min.x + 2.5, b.min.y + 6.8), pos2(b.min.x + 5.2, b.min.y + 9.5), pos2(b.min.x + 10.5, b.min.y + 3.5)];
            p.add(egui::Shape::line(pts, Stroke::new(1.6, tick)));
        }
        p.galley(pos2(b.max.x + 5.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
    }
    r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, *value, text));
    r
}

/// Fader like the C# slider: 3 px rail, blue fill up to the thumb, 12 px light round thumb.
/// Double-click resets to `reset`.
pub fn fader(ui: &mut Ui, id: egui::Id, value: &mut f32, range: std::ops::RangeInclusive<f32>, reset: f32) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
    let mut r = ui.interact(rect, id, Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    let x0 = rect.min.x + 6.0;
    let x1 = rect.max.x - 6.0;
    if r.double_clicked() {
        *value = reset;
        r.mark_changed();
    } else if let Some(p) = r.interact_pointer_pos().filter(|_| r.dragged() || r.clicked()) {
        let t = ((p.x - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
        let v = ((lo + t * (hi - lo)) * 2.0).round() / 2.0;
        if v != *value {
            *value = v;
            r.mark_changed();
        }
    }
    // Read focus before ui.input: calling Response methods inside an input closure re-locks the egui
    // context and deadlocks the UI thread (the app showed "Not Responding" whenever the mouse was over a fader).
    let focused = r.has_focus();
    if r.hovered() || focused {
        let step = ui.input(|i| {
            let mut s = i.raw_scroll_delta.y.signum() * if i.raw_scroll_delta.y != 0.0 { 1.0 } else { 0.0 };
            if focused {
                if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp) {
                    s += 1.0;
                }
                if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown) {
                    s -= 1.0;
                }
            }
            s
        });
        if step != 0.0 {
            *value = (*value + step * 0.5).clamp(lo, hi);
            r.mark_changed();
        }
    }
    if ui.is_rect_visible(rect) {
        let t = ((*value - lo) / (hi - lo)).clamp(0.0, 1.0);
        let tx = x0 + t * (x1 - x0);
        let cy = rect.center().y;
        let p = ui.painter();
        p.rect_filled(Rect::from_min_max(pos2(x0, cy - 1.5), pos2(x1, cy + 1.5)), 1.5, TRACK);
        p.rect_filled(Rect::from_min_max(pos2(x0, cy - 1.5), pos2(tx, cy + 1.5)), 1.5, ACCENT);
        p.circle_filled(pos2(tx, cy), 6.0, TEXT);
    }
    r
}

/// Stereo peak meter: two 4 px bars on a dark track, green to yellow to red gradient across the full width.
pub fn meter(ui: &mut Ui, left: f32, right: f32) {
    ui.add_space(3.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    ui.add_space(2.0);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    p.rect_filled(rect, 1.0, FIELD);
    let green = Color32::from_rgb(0x4F, 0xBF, 0x6B);
    let yellow = Color32::from_rgb(0xE5, 0xC2, 0x3A);
    let red = Color32::from_rgb(0xE0, 0x39, 0x3E);
    let stops = [(0.0f32, green), (0.7, green), (0.85, yellow), (1.0, red)];
    let color_at = |t: f32| {
        for w in stops.windows(2) {
            let ((a, ca), (b, cb)) = (w[0], w[1]);
            if t <= b {
                let k = if b > a { (t - a) / (b - a) } else { 0.0 };
                return ca.lerp_to_gamma(cb, k.clamp(0.0, 1.0));
            }
        }
        red
    };
    let mut mesh = egui::Mesh::default();
    for (i, f) in [left, right].into_iter().enumerate() {
        if f <= 0.0 {
            continue;
        }
        let y0 = rect.min.y + i as f32 * 5.0;
        let y1 = y0 + 4.0;
        let mut xs: Vec<f32> = stops.iter().map(|s| s.0).filter(|&s| s < f).collect();
        xs.push(f);
        for w in xs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (ca, cb) = (color_at(a), color_at(b));
            let (xa, xb) = (rect.min.x + rect.width() * a, rect.min.x + rect.width() * b);
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(pos2(xa, y0), ca);
            mesh.colored_vertex(pos2(xb, y0), cb);
            mesh.colored_vertex(pos2(xb, y1), cb);
            mesh.colored_vertex(pos2(xa, y1), ca);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
    }
    p.add(egui::Shape::mesh(mesh));
}

/// Record button: dark red with a red border and a red dot when idle; solid red with a white dot while recording.
pub fn rec_button(ui: &mut Ui, text: &str, recording: bool, enabled: bool) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let fill = state_fill(&r, if recording { REC } else { REC_BG });
        let p = ui.painter();
        p.rect_filled(rect, 3.0, fade(fill, enabled));
        p.rect_stroke(rect.shrink(0.5), 3.0, Stroke::new(1.0, fade(REC, enabled)), egui::StrokeKind::Middle);
        let galley = p.layout_no_wrap(text.to_owned(), semibold(12.0), fade(TEXT, enabled));
        let w = 10.0 + 8.0 + galley.size().x;
        let x = rect.center().x - w / 2.0;
        p.circle_filled(pos2(x + 5.0, rect.center().y), 5.0, fade(if recording { Color32::WHITE } else { REC }, enabled));
        p.galley(pos2(x + 18.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
    }
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, text));
    r
}

/// List row like the C# ListBoxItem: 2,1 margin, 6,3 padding, 3 px corners, hover and selected fills.
pub fn list_row<R>(ui: &mut Ui, id: egui::Id, selected: bool, add: impl FnOnce(&mut Ui) -> R) -> (Response, R) {
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered() || r.contains_pointer());
    let fill = if selected {
        SELECT
    } else if hovered {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    let inner = egui::Frame::NONE
        .fill(fill)
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .outer_margin(egui::Margin::symmetric(2, 1))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let r = ui.interact(inner.response.rect, id, Sense::click());
    (r, inner.inner)
}

/// Run `add` with combo/text-field styling: dark field fill, 1 px line, lighter line on hover.
pub fn field_style<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for s in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            s.weak_bg_fill = FIELD;
            s.bg_fill = FIELD;
        }
        w.hovered.bg_stroke = Stroke::new(1.0, FIELD_HOVER_LINE);
        w.open.bg_stroke = Stroke::new(1.0, FIELD_HOVER_LINE);
        add(ui)
    })
    .inner
}

/// Chevron painted on the right of a combo box (Segoe MDL2 E70D, 9 px, muted).
pub fn combo_chevron(ui: &Ui, rect: Rect, _v: &egui::style::WidgetVisuals, _open: bool, _dir: egui::AboveOrBelow) {
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "\u{E70D}", icon_font(9.0), MUTED);
}

