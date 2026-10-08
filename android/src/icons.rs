//! Theme-aware vector controls that do not depend on an icon font.
use egui::{Color32, Painter, Pos2, Rect, Response, Stroke, Ui, pos2, vec2};

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Back,
    Up,
    Go,
    Save,
    Sort,
    More,
    Folder,
    File,
    Transfers,
    Desktop,
    Terminal,
    Paste,
    Keyboard,
    Enter,
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, enabled: bool, selected: bool) -> Response {
    badge_button(ui, icon, label, enabled, selected, 0)
}

pub fn badge_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    selected: bool,
    count: usize,
) -> Response {
    let accessible_label = if count == 0 {
        label.to_owned()
    } else {
        format!("{label} ({count})")
    };
    let response = ui
        .add_enabled(
            enabled,
            egui::Button::new("")
                .min_size(vec2(44.0, 44.0))
                .selected(selected),
        )
        .on_hover_text(&accessible_label);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            response.enabled(),
            selected,
            &accessible_label,
        )
    });
    if ui.is_rect_visible(response.rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        paint(
            ui.painter(),
            icon,
            response.rect.center(),
            visuals.fg_stroke.color,
        );
        if count > 0 {
            let text = if count > 99 {
                "99+".into()
            } else {
                count.to_string()
            };
            let width = if count > 99 {
                26.0
            } else if count > 9 {
                22.0
            } else {
                18.0
            };
            let center = pos2(
                response.rect.right() - width * 0.5 - 2.0,
                response.rect.top() + 10.0,
            );
            let rect = Rect::from_center_size(center, vec2(width, 18.0));
            let selection = ui.visuals().selection;
            ui.painter().rect_filled(rect, 9.0, selection.bg_fill);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(11.0),
                selection.stroke.color,
            );
        }
    }
    response
}

pub fn menu(ui: &mut Ui, icon: Icon, label: &str, contents: impl FnOnce(&mut Ui)) -> Response {
    let response = button(ui, icon, label, true, false);
    egui::Popup::menu(&response).show(contents);
    response
}

pub fn small(ui: &mut Ui, icon: Icon) {
    let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), icon, rect.center(), ui.visuals().text_color());
    }
}

fn paint(painter: &Painter, icon: Icon, center: Pos2, color: Color32) {
    // Draw on a 24-unit canvas scaled to a 22px glyph.
    let point = |x: f32, y: f32| center + vec2(x - 12.0, y - 12.0) * (22.0 / 24.0);
    let stroke = Stroke::new(1.9, color);
    let line = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(
            points.iter().map(|&(x, y)| point(x, y)).collect(),
            stroke,
        ));
    };
    let rect = |left, top, right, bottom, rounding| {
        painter.rect_stroke(
            Rect::from_min_max(point(left, top), point(right, bottom)),
            rounding,
            stroke,
            egui::StrokeKind::Inside,
        );
    };
    match icon {
        Icon::Back => {
            line(&[(10.0, 4.0), (2.0, 12.0), (10.0, 20.0)]);
            line(&[(2.0, 12.0), (22.0, 12.0)]);
        }
        Icon::Up => {
            line(&[(4.0, 10.0), (12.0, 2.0), (20.0, 10.0)]);
            line(&[(12.0, 2.0), (12.0, 22.0)]);
        }
        Icon::Go => {
            line(&[(14.0, 4.0), (22.0, 12.0), (14.0, 20.0)]);
            line(&[(2.0, 12.0), (22.0, 12.0)]);
        }
        Icon::Save => {
            line(&[(12.0, 2.0), (12.0, 15.0)]);
            line(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)]);
            line(&[(3.0, 14.0), (3.0, 21.0), (21.0, 21.0), (21.0, 14.0)]);
        }
        Icon::Sort => {
            line(&[(5.0, 3.0), (5.0, 21.0)]);
            line(&[(1.0, 17.0), (5.0, 21.0), (9.0, 17.0)]);
            line(&[(12.0, 5.0), (23.0, 5.0)]);
            line(&[(12.0, 11.0), (20.0, 11.0)]);
            line(&[(12.0, 17.0), (17.0, 17.0)]);
        }
        Icon::More => {
            for y in [4.0, 12.0, 20.0] {
                painter.circle_filled(point(12.0, y), 1.9, color);
            }
        }
        Icon::Folder => line(&[
            (2.0, 20.0),
            (2.0, 4.0),
            (9.0, 4.0),
            (12.0, 7.0),
            (22.0, 7.0),
            (22.0, 20.0),
            (2.0, 20.0),
        ]),
        Icon::File => {
            line(&[
                (5.0, 2.0),
                (14.0, 2.0),
                (20.0, 8.0),
                (20.0, 22.0),
                (5.0, 22.0),
                (5.0, 2.0),
            ]);
            line(&[(14.0, 2.0), (14.0, 8.0), (20.0, 8.0)]);
            line(&[(9.0, 13.0), (16.0, 13.0)]);
            line(&[(9.0, 17.0), (16.0, 17.0)]);
        }
        Icon::Transfers => {
            line(&[(7.0, 2.0), (7.0, 22.0)]);
            line(&[(2.0, 7.0), (7.0, 2.0), (12.0, 7.0)]);
            line(&[(17.0, 2.0), (17.0, 22.0)]);
            line(&[(12.0, 17.0), (17.0, 22.0), (22.0, 17.0)]);
        }
        Icon::Desktop => {
            rect(2.0, 3.0, 22.0, 17.0, 2.0);
            line(&[(12.0, 17.0), (12.0, 22.0)]);
            line(&[(7.0, 22.0), (17.0, 22.0)]);
        }
        Icon::Terminal => {
            rect(1.0, 3.0, 23.0, 21.0, 2.0);
            line(&[(5.0, 8.0), (9.0, 12.0), (5.0, 16.0)]);
            line(&[(12.0, 16.0), (19.0, 16.0)]);
        }
        Icon::Keyboard => {
            rect(1.0, 5.0, 23.0, 20.0, 2.0);
            for y in [9.0, 12.0] {
                for x in [5.0, 9.5, 14.0, 18.5] {
                    painter.circle_filled(point(x, y), 0.9, color);
                }
            }
            line(&[(7.0, 16.0), (17.0, 16.0)]);
        }
        Icon::Enter => {
            line(&[(21.0, 4.0), (21.0, 14.0), (3.0, 14.0)]);
            line(&[(9.0, 8.0), (3.0, 14.0), (9.0, 20.0)]);
        }
        Icon::Paste => {
            line(&[
                (8.0, 4.0),
                (4.0, 4.0),
                (4.0, 22.0),
                (20.0, 22.0),
                (20.0, 4.0),
                (16.0, 4.0),
            ]);
            rect(8.0, 1.0, 16.0, 7.0, 1.0);
            line(&[(8.0, 12.0), (16.0, 12.0)]);
            line(&[(8.0, 17.0), (16.0, 17.0)]);
        }
    }
}
