//! The small painted pieces every screen shares: text, buttons, panels, refusals and the
//! readout's facts. egui's stock widgets don't look like the design, so these draw themselves.

use std::sync::Arc;

use bootshift_core::refusal::Refusal;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, Galley, Margin, Painter, Pos2, Rect, Response,
    Sense, Stroke, StrokeKind, Ui, Vec2,
    text::{LayoutJob, TextFormat, TextWrapping},
};

use crate::theme::{self, Face, Weight, size};

pub fn galley(
    ui: &Ui,
    text: impl Into<String>,
    face: Face,
    px: f32,
    colour: Color32,
) -> Arc<Galley> {
    ui.painter()
        .layout_no_wrap(text.into(), face.at(px), colour)
}

/// One line of text, cut short with an ellipsis when it would pass `max_width`.
pub fn elided(
    ui: &Ui,
    text: impl Into<String>,
    face: Face,
    px: f32,
    colour: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.into(), format(face, px, colour));
    job.wrap = TextWrapping {
        max_width: max_width.max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// Paragraph text wrapped to `width`, with the design's line height.
pub fn wrapped(
    ui: &Ui,
    text: impl Into<String>,
    face: Face,
    px: f32,
    colour: Color32,
    width: f32,
    line_height: f32,
) -> Arc<Galley> {
    let mut format = format(face, px, colour);
    format.line_height = Some(px * line_height);
    let mut job = LayoutJob::single_section(text.into(), format);
    job.wrap.max_width = width.max(1.0);
    ui.painter().layout_job(job)
}

pub fn format(face: Face, px: f32, colour: Color32) -> TextFormat {
    TextFormat {
        font_id: face.at(px),
        color: colour,
        ..TextFormat::default()
    }
}

/// Lays out and paints text as an ordinary widget, taking the room it needs.
pub fn label(
    ui: &mut Ui,
    text: impl Into<String>,
    face: Face,
    px: f32,
    colour: Color32,
) -> Response {
    let galley = galley(ui, text, face, px, colour);
    show_galley(ui, galley)
}

pub fn paragraph(
    ui: &mut Ui,
    text: impl Into<String>,
    px: f32,
    colour: Color32,
    width: f32,
    line_height: f32,
) -> Response {
    let galley = wrapped(ui, text, theme::SANS, px, colour, width, line_height);
    show_galley(ui, galley)
}

pub fn show_galley(ui: &mut Ui, galley: Arc<Galley>) -> Response {
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, theme::INK);
    response
}

pub fn text_at(painter: &Painter, anchor: Align2, pos: Pos2, galley: Arc<Galley>) -> Rect {
    let rect = anchor.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, theme::INK);
    rect
}

/// A check mark `size` points across, centred on `centre`. Drawn rather than typed, because
/// none of the bundled faces has a ✓.
pub fn tick(painter: &Painter, centre: Pos2, size: f32, colour: Color32, width: f32) {
    let s = size / 2.0;
    painter.line(
        vec![
            centre + Vec2::new(-s, 0.05 * s),
            centre + Vec2::new(-0.3 * s, 0.7 * s),
            centre + Vec2::new(s, -0.65 * s),
        ],
        Stroke::new(width, colour),
    );
}

/// A cross `size` points across, centred on `centre`.
pub fn cross(painter: &Painter, centre: Pos2, size: f32, colour: Color32, width: f32) {
    let s = size / 2.0;
    let stroke = Stroke::new(width, colour);
    painter.line_segment(
        [centre + Vec2::new(-s, -s), centre + Vec2::new(s, s)],
        stroke,
    );
    painter.line_segment(
        [centre + Vec2::new(-s, s), centre + Vec2::new(s, -s)],
        stroke,
    );
}

/// The keyboard focus ring: 2px of amber, 2px outside the thing focused.
pub fn focus_ring(painter: &Painter, rect: Rect, radius: u8) {
    painter.rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius.saturating_add(2)),
        Stroke::new(2.0, theme::SIGNAL),
        StrokeKind::Outside,
    );
}

/// Whether focus came from the keyboard, so a mouse click doesn't leave a ring behind.
pub fn shows_focus(ui: &Ui, response: &Response) -> bool {
    response.has_focus() && !ui.input(|i| i.pointer.any_down())
}

const SECONDARY_HOVER: Color32 = Color32::from_rgb(0x24, 0x24, 0x2b);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Secondary,
    Primary,
}

pub fn button(ui: &mut Ui, text: &str, kind: Kind, enabled: bool) -> Response {
    let weight = if kind == Kind::Primary {
        Weight::SemiBold
    } else {
        Weight::Medium
    };
    let probe = galley(ui, text, Face::Sans(weight), size::BUTTON, theme::INK);
    let desired = Vec2::new(probe.size().x + 32.0, probe.size().y + 14.0).round();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(desired, sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, text));
    let hover = enabled && response.hovered();
    let (fill, border, ink) = match (enabled, kind) {
        (false, _) => (theme::DECK, theme::RULE_SOFT, theme::FAINT),
        (true, Kind::Primary) if hover => {
            (theme::SIGNAL_HOVER, theme::SIGNAL_HOVER, theme::SIGNAL_INK)
        }
        (true, Kind::Primary) => (theme::SIGNAL, theme::SIGNAL, theme::SIGNAL_INK),
        (true, Kind::Secondary) if hover => (SECONDARY_HOVER, theme::RULE_HOVER, theme::INK),
        (true, Kind::Secondary) => (theme::DECK_HIGH, theme::RULE, theme::INK),
    };
    let painter = ui.painter();
    let fill = animate_colour(ui, response.id.with("fill"), fill);
    painter.rect_filled(rect, 3, fill);
    painter.rect_stroke(rect, 3, Stroke::new(1.0, border), StrokeKind::Inside);
    let label = galley(ui, text, Face::Sans(weight), size::BUTTON, ink);
    text_at(painter, Align2::CENTER_CENTER, rect.center(), label);
    if shows_focus(ui, &response) {
        focus_ring(painter, rect, 3);
    }
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// Fades a colour toward `target` over the design's 0.12s hover time.
fn animate_colour(ui: &Ui, id: egui::Id, target: Color32) -> Color32 {
    let ctx = ui.ctx();
    let [r, g, b, a] = target.to_array().map(f32::from);
    let time = if crate::motion::reduced() { 0.0 } else { 0.12 };
    let channel =
        |n: &str, value: f32| ctx.animate_value_with_time(id.with(n), value, time).round() as u8;
    Color32::from_rgba_premultiplied(
        channel("r", r),
        channel("g", g),
        channel("b", b),
        channel("a", a),
    )
}

/// A panel: deck background, a 1px rule, 4px corners and a faint highlight along the top edge,
/// which is what makes it read as lit from above.
pub fn panel<R>(ui: &mut Ui, margin: Margin, add: impl FnOnce(&mut Ui) -> R) -> R {
    let inner = egui::Frame::new()
        .fill(theme::DECK)
        .stroke(Stroke::new(1.0, theme::RULE))
        .corner_radius(4)
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    let rect = inner.response.rect;
    ui.painter().hline(
        (rect.left() + 3.0)..=(rect.right() - 3.0),
        rect.top() + 1.5,
        Stroke::new(1.0, theme::alpha(Color32::WHITE, 0.03)),
    );
    inner.inner
}

/// Every refusal with its reason and its fix. A refusal is never shown without its fix.
pub fn refusals(ui: &mut Ui, refusals: &[Refusal]) {
    for (index, refusal) in refusals.iter().enumerate() {
        if index > 0 {
            ui.add_space(8.0);
        }
        refusal_box(ui, refusal);
    }
}

fn refusal_box(ui: &mut Ui, refusal: &Refusal) {
    let inner = egui::Frame::new()
        .fill(theme::alpha(theme::STOP, 0.05))
        .stroke(Stroke::new(1.0, theme::alpha(theme::STOP, 0.30)))
        .corner_radius(3)
        .inner_margin(Margin {
            left: 16,
            right: 14,
            top: 11,
            bottom: 11,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let width = ui.available_width();
            paragraph(ui, &refusal.reason, 13.0, theme::INK, width, 1.45);
            ui.add_space(5.0);
            let mut job = LayoutJob::default();
            job.append(
                "Fix: ",
                0.0,
                format(Face::Sans(Weight::SemiBold), 12.0, theme::INK),
            );
            job.append(&refusal.fix, 0.0, format(theme::SANS, 12.0, theme::DIM));
            job.wrap.max_width = width;
            let fix = ui.painter().layout_job(job);
            show_galley(ui, fix);
        });
    let rect = inner.response.rect;
    let edge = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
    ui.painter().rect_filled(
        edge,
        CornerRadius {
            nw: 3,
            sw: 3,
            ne: 0,
            se: 0,
        },
        theme::STOP,
    );
}

/// How a readout value reads: plain, changed from today (amber), or good news (green).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Changed,
    Good,
}

pub fn readout_heading(ui: &mut Ui, text: &str) {
    label(
        ui,
        text,
        Face::Sans(Weight::SemiBold),
        size::HEADING,
        theme::DIM,
    );
    ui.add_space(10.0);
}

pub fn readout_text(ui: &mut Ui, text: &str) {
    let width = ui.available_width();
    paragraph(ui, text, 12.0, theme::DIM, width, 1.55);
}

pub fn readout_texts(ui: &mut Ui, texts: &[&str]) {
    for (index, text) in texts.iter().enumerate() {
        if index > 0 {
            ui.add_space(9.0);
        }
        readout_text(ui, text);
    }
}

/// Key on the left, value right-aligned in mono, a soft divider under all but the last row.
pub fn facts(ui: &mut Ui, rows: &[(&str, String, Tone)]) {
    let width = ui.available_width();
    for (index, (key, value, tone)) in rows.iter().enumerate() {
        let colour = match tone {
            Tone::Plain => theme::INK,
            Tone::Changed => theme::SIGNAL,
            Tone::Good => theme::VERIFIED,
        };
        let value = galley(ui, value.clone(), theme::MONO, size::DATA, colour);
        let key_width = (width - value.size().x - 11.0).max(30.0);
        let key = elided(ui, *key, theme::SANS, 11.7, theme::FAINT, key_width);
        let height = key.size().y.max(value.size().y) + 10.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter();
        text_at(painter, Align2::LEFT_CENTER, rect.left_center(), key);
        text_at(painter, Align2::RIGHT_CENTER, rect.right_center(), value);
        if index + 1 < rows.len() {
            painter.hline(
                rect.x_range(),
                rect.bottom() - 0.5,
                Stroke::new(1.0, theme::RULE_SOFT),
            );
        }
    }
}

/// The gap between readout blocks.
pub fn block_gap(ui: &mut Ui) {
    ui.add_space(21.0);
}
