//! The four fixed regions: the rail, the work area, the readout and the action bar. The window
//! never scrolls as a whole on a wide screen; the work area and the panels scroll on their own.
//! Narrower windows move the readout under the work area, then fold the rail into a strip.

use eframe::egui::{
    self, Align, Align2, Color32, ComboBox, CursorIcon, Layout, Margin, Mesh, Panel, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::app::{Action, App, Screen};
use crate::readout;
use crate::screens;
use crate::theme::{self, Face, Weight, layout};
use crate::widgets::{self, Kind};

pub fn show(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) {
    let width = ui.ctx().content_rect().width();
    let narrow = width < layout::NARROW_BELOW;
    let stacked = width < layout::STACK_READOUT_BELOW;

    // The migration goal is its own full-window flow: no partition rail, readout or disk.
    if app.screen == crate::app::Screen::Migrate {
        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0, theme::VOID);
            let side = if narrow { 16 } else { 40 };
            egui::Frame::new()
                .inner_margin(Margin::symmetric(side, 26))
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("migrate-scroll")
                        .show(ui, |ui| {
                            ui.set_max_width(760.0);
                            crate::migrate::show(ui, &app.migrate, actions, narrow);
                        });
                });
        });
        return;
    }

    if narrow {
        Panel::top("rail")
            .resizable(false)
            .frame(rail_frame(Margin::symmetric(15, 8)))
            .show(ui, |ui| rail_strip(ui, app, actions));
    } else {
        Panel::left("rail")
            .resizable(false)
            .exact_size(layout::RAIL)
            .frame(rail_frame(Margin::ZERO))
            .show(ui, |ui| rail(ui, app, actions));
    }
    let side = if narrow { 15 } else { 34 };
    Panel::bottom("actions")
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(Margin::symmetric(side, 13)))
        .show(ui, |ui| action_bar(ui, app, actions));
    if !stacked {
        Panel::right("readout")
            .resizable(false)
            .exact_size(layout::READOUT)
            .frame(
                egui::Frame::new()
                    .fill(theme::DECK)
                    .inner_margin(Margin::symmetric(19, 22)),
            )
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt("readout-scroll")
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        readout::show(ui, app);
                    });
            });
    }
    egui::CentralPanel::no_frame().show(ui, |ui| {
        ui.painter().rect_filled(ui.max_rect(), 0, theme::VOID);
        work(ui, app, actions, narrow, stacked);
    });
    borders(ui, narrow, stacked);
}

fn rail_frame(margin: Margin) -> egui::Frame {
    egui::Frame::new().fill(theme::DECK).inner_margin(margin)
}

/// The 1px rules between regions, drawn over the panels so they meet cleanly.
fn borders(ui: &Ui, narrow: bool, stacked: bool) {
    let screen = ui.ctx().content_rect();
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("region-borders"),
    ));
    let rule = Stroke::new(1.0, theme::RULE);
    let rail = ui.ctx().memory(|m| m.area_rect(egui::Id::unique("rail")));
    let readout = ui
        .ctx()
        .memory(|m| m.area_rect(egui::Id::unique("readout")));
    let actions = ui
        .ctx()
        .memory(|m| m.area_rect(egui::Id::unique("actions")));
    if let Some(rect) = rail {
        if narrow {
            painter.hline(screen.x_range(), rect.bottom() - 0.5, rule);
        } else {
            painter.vline(rect.right() - 0.5, screen.y_range(), rule);
        }
    }
    if let (Some(rect), false) = (readout, stacked) {
        painter.vline(rect.left() + 0.5, rect.y_range(), rule);
    }
    if let Some(rect) = actions {
        painter.hline(rect.x_range(), rect.top() + 0.5, rule);
    }
}

/* ---------- rail ---------- */

fn rail(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let full = ui.available_rect_before_wrap();
    egui::Frame::new()
        .inner_margin(Margin {
            left: 19,
            right: 19,
            top: 17,
            bottom: 16,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            brand(ui);
        });
    let below_brand = ui.min_rect().bottom();
    ui.painter().hline(
        full.x_range(),
        below_brand - 0.5,
        Stroke::new(1.0, theme::RULE_SOFT),
    );

    egui::Frame::new()
        .inner_margin(Margin::symmetric(9, 13))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            goals_tab(ui, app, true, actions);
            ui.add_space(10.0);
            for (index, screen) in Screen::ALL.into_iter().enumerate() {
                if index > 0 {
                    ui.add_space(1.0);
                }
                step(ui, app, screen, index, true, actions);
            }
        });

    // The disk picker sits at the foot of the rail, under a soft rule. Its height is known
    // from the frame before, so the gap above it can push it to the bottom of the window.
    let foot_id = egui::Id::unique("rail-foot-height");
    let foot_height: f32 = ui.data(|d| d.get_temp(foot_id)).unwrap_or(0.0);
    let room = ui.max_rect().bottom() - ui.cursor().top() - foot_height;
    ui.add_space(room.max(0.0));
    let foot = egui::Frame::new()
        .inner_margin(Margin {
            left: 19,
            right: 19,
            top: 15,
            bottom: 17,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::label(ui, "Disk", theme::SANS, 11.0, theme::FAINT);
            ui.add_space(5.0);
            disk_picker(ui, app, actions);
            if app.is_demo() {
                ui.add_space(12.0);
                demo_badge(ui);
            }
        });
    let rect = foot.response.rect;
    ui.data_mut(|d| d.insert_temp(foot_id, rect.height()));
    ui.painter().hline(
        full.x_range(),
        rect.top() + 0.5,
        Stroke::new(1.0, theme::RULE_SOFT),
    );
}

fn rail_strip(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(15.0, 8.0);
        brand(ui);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            goals_tab(ui, app, false, actions);
            for (index, screen) in Screen::ALL.into_iter().enumerate() {
                step(ui, app, screen, index, false, actions);
            }
        });
        ui.allocate_ui(Vec2::new(200.0, 30.0), |ui| disk_picker(ui, app, actions));
    });
}

/// Decodes the bundled Kitten logo once and caches the uploaded texture in egui memory, so
/// `brand` can paint it every frame without re-decoding.
fn logo_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::NULL.with("kitten-logo");
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    // The logo is bundled at build time, so decoding should never fail; if it somehow did, fall
    // back to a blank pixel rather than panicking.
    let color = match image::load_from_memory(include_bytes!("../../../assets/logo.png")) {
        Ok(decoded) => {
            let rgba = decoded.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
        }
        Err(_) => egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 0]),
    };
    let tex = ctx.load_texture("kitten-logo", color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// The Kitten logo as a small app-icon tile, then the word mark.
fn brand(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 9.0;
        let tex = logo_texture(ui.ctx());
        let (badge, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        // The logo is a white cat on a transparent ground, so it needs a dark app-icon badge
        // to read on the light rail. A small inset keeps the green halo clear of the corners.
        ui.painter()
            .rect_filled(badge, 6, Color32::from_rgb(0x1b, 0x1a, 0x17));
        ui.painter().image(
            tex.id(),
            badge.shrink(2.0),
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );

        let mut job = egui::text::LayoutJob::default();
        let mut kit = widgets::format(Face::Sans(Weight::Medium), 15.3, theme::DIM);
        kit.extra_letter_spacing = -0.23;
        let mut ten = widgets::format(Face::Sans(Weight::Bold), 15.3, theme::INK);
        ten.extra_letter_spacing = -0.23;
        job.append("Kit", 0.0, kit);
        job.append("ten", 0.0, ten);
        let galley = ui.painter().layout_job(job);
        widgets::show_galley(ui, galley);
    });
}

fn step(
    ui: &mut Ui,
    app: &App,
    screen: Screen,
    index: usize,
    labelled: bool,
    actions: &mut Vec<Action>,
) {
    let current = screen == app.screen;
    let passed = screen < app.screen;
    let reachable = screen <= app.reached && !current;
    let weight = if current {
        Weight::SemiBold
    } else {
        Weight::Regular
    };
    let width = if labelled {
        ui.available_width()
    } else {
        21.0 + 20.0
    };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, 35.0),
        if reachable {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::Role::Tab,
            reachable || current,
            current,
            screen.name(),
        )
    });
    if reachable && response.clicked() {
        actions.push(Action::Show(screen));
    }
    let hovered = reachable && response.hovered();
    let painter = ui.painter();
    if current || hovered {
        painter.rect_filled(rect, 3, theme::DECK_HIGH);
    }
    let circle = Rect::from_center_size(
        Pos2::new(rect.left() + 10.0 + 10.5, rect.center().y),
        Vec2::splat(21.0),
    );
    if labelled && index > 0 {
        painter.vline(
            circle.center().x,
            (rect.top() - 5.0)..=(rect.top() + 2.0),
            Stroke::new(1.0, theme::RULE),
        );
    }
    let (fill, border, number) = if current {
        (theme::SIGNAL, theme::SIGNAL, theme::SIGNAL_INK)
    } else if passed {
        (
            theme::VOID,
            theme::alpha(theme::VERIFIED, 0.4),
            theme::VERIFIED,
        )
    } else if reachable {
        (theme::VOID, theme::RULE, theme::DIM)
    } else {
        (theme::VOID, theme::RULE, theme::FAINT)
    };
    painter.circle_filled(circle.center(), 10.5, fill);
    painter.circle_stroke(circle.center(), 10.0, Stroke::new(1.0, border));
    let number_face = Face::Mono(if current {
        Weight::Bold
    } else {
        Weight::Regular
    });
    let digit = widgets::galley(ui, (index + 1).to_string(), number_face, 10.2, number);
    widgets::text_at(painter, Align2::CENTER_CENTER, circle.center(), digit);
    if labelled {
        let colour = if current || hovered {
            theme::INK
        } else if reachable {
            theme::DIM
        } else {
            theme::FAINT
        };
        let text = widgets::galley(ui, screen.name(), Face::Sans(weight), 13.0, colour);
        widgets::text_at(
            painter,
            Align2::LEFT_CENTER,
            Pos2::new(circle.right() + 9.0, rect.center().y),
            text,
        );
    }
    if widgets::shows_focus(ui, &response) {
        widgets::focus_ring(painter, rect, 3);
    }
    if reachable {
        response.on_hover_cursor(CursorIcon::PointingHand);
    }
}

/// The Goals launcher, shown above the numbered steps. It isn't one of the five steps, so it
/// wears a grid mark instead of a number and is always reachable: you can go back to the goals
/// from anywhere in the flow.
fn goals_tab(ui: &mut Ui, app: &App, labelled: bool, actions: &mut Vec<Action>) {
    let current = app.screen == Screen::Home;
    let width = if labelled {
        ui.available_width()
    } else {
        21.0 + 20.0
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 35.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, current, "Goals"));
    if response.clicked() && !current {
        actions.push(Action::Show(Screen::Home));
    }
    let hovered = response.hovered() && !current;
    let painter = ui.painter();
    if current || hovered {
        painter.rect_filled(rect, 3, theme::DECK_HIGH);
    }
    let icon = Rect::from_center_size(
        Pos2::new(rect.left() + 10.0 + 10.5, rect.center().y),
        Vec2::splat(21.0),
    );
    let (fill, border, glyph) = if current {
        (theme::SIGNAL, theme::SIGNAL, theme::SIGNAL_INK)
    } else if hovered {
        (theme::VOID, theme::RULE_HOVER, theme::INK)
    } else {
        (theme::VOID, theme::RULE, theme::DIM)
    };
    painter.circle_filled(icon.center(), 10.5, fill);
    painter.circle_stroke(icon.center(), 10.0, Stroke::new(1.0, border));
    grid_glyph(painter, icon.center(), glyph);
    if labelled {
        let colour = if current || hovered {
            theme::INK
        } else {
            theme::DIM
        };
        let weight = if current {
            Weight::SemiBold
        } else {
            Weight::Regular
        };
        let text = widgets::galley(ui, "Goals", Face::Sans(weight), 13.0, colour);
        widgets::text_at(
            painter,
            Align2::LEFT_CENTER,
            Pos2::new(icon.right() + 9.0, rect.center().y),
            text,
        );
    }
    if widgets::shows_focus(ui, &response) {
        widgets::focus_ring(painter, rect, 3);
    }
    response.on_hover_cursor(CursorIcon::PointingHand);
}

/// A 2x2 of small squares: the "all the goal cards" mark.
fn grid_glyph(painter: &egui::Painter, centre: Pos2, colour: Color32) {
    let offset = 2.7;
    for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let c = Pos2::new(centre.x + dx * offset, centre.y + dy * offset);
        painter.rect_filled(Rect::from_center_size(c, Vec2::splat(3.2)), 1, colour);
    }
}

fn disk_picker(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) -> egui::Response {
    let mono = |text: String| {
        RichText::new(text)
            .font(theme::MONO.at(11.5))
            .color(theme::INK)
    };
    let selected = app.probed.as_ref().map(|p| p.disk.clone());
    let current = app
        .disks
        .iter()
        .find(|d| Some(&d.path) == selected.as_ref())
        .map_or_else(String::new, disk_label);
    let response = ComboBox::from_id_salt("disk")
        .width(ui.available_width())
        .wrap_mode(egui::TextWrapMode::Truncate)
        .selected_text(mono(current))
        .show_ui(ui, |ui| {
            for disk in &app.disks {
                let chosen = Some(&disk.path) == selected.as_ref();
                if ui
                    .selectable_label(chosen, mono(disk_label(disk)))
                    .clicked()
                    && !chosen
                {
                    actions.push(Action::ChooseDisk(disk.path.clone()));
                }
            }
        })
        .response;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Disk"));
    response
}

fn disk_label(disk: &crate::protocol::DiskSummary) -> String {
    format!(
        "{}  {}",
        disk.path,
        bootshift_core::units::format_bytes(disk.size)
    )
}

fn demo_badge(ui: &mut Ui) {
    egui::Frame::new()
        .fill(theme::alpha(theme::SIGNAL, 0.06))
        .stroke(Stroke::new(1.0, theme::alpha(theme::SIGNAL, 0.25)))
        .corner_radius(3)
        .inner_margin(Margin::symmetric(9, 7))
        .show(ui, |ui| {
            let width = ui.available_width();
            widgets::paragraph(
                ui,
                "Demo disk. Nothing here touches hardware.",
                10.8,
                theme::SIGNAL,
                width,
                1.4,
            );
        });
}

/* ---------- work area ---------- */

fn work(ui: &mut Ui, app: &App, actions: &mut Vec<Action>, narrow: bool, stacked: bool) {
    let side = if narrow { 15.0 } else { 34.0 };
    let viewport = ui.available_rect_before_wrap();
    let content_id = egui::Id::unique("work-content-height");
    let last_height: f32 = ui.data(|d| d.get_temp(content_id)).unwrap_or(0.0);
    ScrollArea::vertical()
        .id_salt("work-scroll")
        .auto_shrink(false)
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(Margin {
                    left: side as i8,
                    right: side as i8,
                    top: 30,
                    bottom: 22,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Centre a short screen in the area; a tall one starts at the top and scrolls.
                    let room = viewport.height() - 52.0;
                    if !stacked && last_height > 0.0 && last_height < room {
                        ui.add_space(((room - last_height) / 2.0).floor());
                    }
                    let start = ui.min_rect().bottom();
                    transition(ui, app, |ui| screens::show(ui, app, actions, narrow));
                    let height = ui.min_rect().bottom() - start;
                    ui.data_mut(|d| d.insert_temp(content_id, height));
                });
            if stacked {
                let rect = ui.available_rect_before_wrap();
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    Stroke::new(1.0, theme::RULE),
                );
                egui::Frame::new()
                    .fill(theme::DECK)
                    .inner_margin(Margin::symmetric(side as i8, 15))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        readout::show(ui, app);
                    });
            }
        });
}

/// A screen fades in and rises 5px over 0.2s when it changes, unless motion is reduced.
fn transition(ui: &mut Ui, app: &App, add: impl FnOnce(&mut Ui)) {
    let progress = if crate::motion::reduced() {
        1.0
    } else {
        ((app.now() - app.screen_shown_at) / 0.2).clamp(0.0, 1.0) as f32
    };
    if progress < 1.0 {
        ui.ctx().request_repaint();
    }
    let eased = 1.0 - (1.0 - progress).powi(3);
    ui.scope(|ui| {
        ui.multiply_opacity(eased);
        let rise = 5.0 * (1.0 - eased);
        if rise > 0.0 {
            ui.add_space(rise);
        }
        add(ui);
    });
}

/* ---------- action bar ---------- */

fn action_bar(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) {
    let full = ui.max_rect().expand2(Vec2::new(40.0, 13.0));
    gradient(ui, full, theme::DECK, theme::VOID);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        buttons(ui, app, actions);
        ui.add_space(6.0);
        let width = ui.available_width();
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let status =
                widgets::elided(ui, app.status.clone(), theme::SANS, 12.3, theme::DIM, width);
            widgets::show_galley(ui, status);
        });
    });
}

/// Right to left: the primary button is laid out first, so it sits furthest right.
fn buttons(ui: &mut Ui, app: &App, actions: &mut Vec<Action>) {
    let mut push = |ui: &mut Ui, text: &str, kind: Kind, enabled: bool, action: Action| {
        if widgets::button(ui, text, kind, enabled).clicked() {
            actions.push(action);
        }
    };
    match app.screen {
        // The home screen's actions are the goal cards themselves.
        Screen::Home => {}
        // The migrate screen is full-window with its own controls.
        Screen::Migrate => {}
        Screen::Arrange => {
            push(
                ui,
                "See the plan",
                Kind::Primary,
                app.has_changes(),
                Action::Show(Screen::Plan),
            );
            // On a goal screen, the way back is to the goals; in the editor, it's Start over.
            if app.goal.is_some_and(|g| g != crate::app::Goal::DoItMyself) {
                push(
                    ui,
                    "Back to goals",
                    Kind::Secondary,
                    true,
                    Action::Show(Screen::Home),
                );
            } else {
                push(
                    ui,
                    "Start over",
                    Kind::Secondary,
                    app.has_changes(),
                    Action::StartOver,
                );
            }
        }
        Screen::Plan => {
            push(
                ui,
                "Run the safety checks",
                Kind::Primary,
                app.plannable(),
                Action::RunChecks,
            );
            push(
                ui,
                "Adjust the layout",
                Kind::Secondary,
                true,
                Action::Show(Screen::Arrange),
            );
        }
        Screen::Checks => {
            if app.prepared_to_restart {
                // The change is written; the only thing left is to restart. Offer a way back to
                // the goals, but no apply button to press again.
                push(
                    ui,
                    "Back to goals",
                    Kind::Secondary,
                    true,
                    Action::Show(Screen::Home),
                );
            } else {
                push(
                    ui,
                    "Apply and restart later",
                    Kind::Primary,
                    app.can_apply(),
                    Action::Apply,
                );
                push(ui, "Check again", Kind::Secondary, true, Action::RunChecks);
            }
        }
        Screen::Done => {
            push(
                ui,
                "Plan another change",
                Kind::Primary,
                true,
                Action::PlanAnother,
            );
        }
    }
}

fn gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    ui.painter().add(mesh);
}

/// A row with a checkbox and its text centred on each other; the whole row is one target.
pub fn checkbox_row(ui: &mut Ui, checked: bool, text: &str) -> egui::Response {
    let galley = widgets::galley(ui, text, theme::SANS, 13.2, theme::INK);
    let size = Vec2::new(16.0 + 9.0 + galley.size().x, galley.size().y.max(16.0));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, checked, text));
    let painter = ui.painter();
    let box_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 8.0, rect.center().y),
        Vec2::splat(16.0),
    );
    if checked {
        painter.rect_filled(box_rect, 3, theme::SIGNAL);
        let tick = [
            Pos2::new(box_rect.left() + 3.5, box_rect.center().y + 0.5),
            Pos2::new(box_rect.left() + 6.8, box_rect.bottom() - 4.0),
            Pos2::new(box_rect.right() - 3.5, box_rect.top() + 4.0),
        ];
        painter.line(tick.to_vec(), Stroke::new(2.0, theme::SIGNAL_INK));
    } else {
        painter.rect_filled(box_rect, 3, theme::VOID);
        let border = if response.hovered() {
            theme::SIGNAL
        } else {
            theme::RULE_HOVER
        };
        painter.rect_stroke(box_rect, 3, Stroke::new(1.0, border), StrokeKind::Inside);
    }
    widgets::text_at(
        painter,
        Align2::LEFT_CENTER,
        Pos2::new(box_rect.right() + 9.0, rect.center().y),
        galley,
    );
    if widgets::shows_focus(ui, &response) {
        widgets::focus_ring(painter, box_rect, 3);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}
