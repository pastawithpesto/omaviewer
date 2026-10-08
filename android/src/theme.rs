use eframe::egui::{self, Color32, CornerRadius, Shadow, Stroke};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    TokyoNight,
    Catppuccin,
    Gruvbox,
    RosePine,
}

impl Theme {
    pub const ALL: [Self; 4] = [
        Self::TokyoNight,
        Self::Catppuccin,
        Self::Gruvbox,
        Self::RosePine,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::Gruvbox => "Gruvbox",
            Self::RosePine => "Rosé Pine",
        }
    }

    pub fn palette(self) -> Palette {
        // Mapped from Omarchy colors.toml. `muted` is secondary label text
        // (light_foreground, darkened on Rosé Pine for page contrast) and
        // `border` is dark_foreground so 1px field outlines stay visible.
        let (dark, background, surface, raised, accent, text, muted, border, green) = match self {
            Self::TokyoNight => (
                true, 0x0e0e14, 0x1a1b26, 0x24283b, 0x7aa2f7, 0xc0caf5, 0x8a93b6, 0x565f89,
                0x9ece6a,
            ),
            Self::Catppuccin => (
                true, 0x101019, 0x1e1e2e, 0x313244, 0x89b4fa, 0xcdd6f4, 0xa6adc8, 0x6c7086,
                0xa6e3a1,
            ),
            Self::Gruvbox => (
                true, 0x161616, 0x282828, 0x3c3836, 0x7daea3, 0xd4be98, 0xbdae93, 0x7c6f64,
                0xa9b665,
            ),
            Self::RosePine => (
                false, 0xe1dbd5, 0xfaf4ed, 0xf2e9e1, 0x56949f, 0x575279, 0x5c5878, 0x9893a5,
                0x286983,
            ),
        };
        Palette {
            dark,
            background: rgb(background),
            surface: rgb(surface),
            raised: rgb(raised),
            accent: rgb(accent),
            text: rgb(text),
            muted: rgb(muted),
            border: rgb(border),
            green: rgb(green),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub background: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub accent: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub border: Color32,
    pub green: Color32,
}

fn rgb(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t)
}

fn paint_widget(
    visuals: &mut egui::style::WidgetVisuals,
    fill: Color32,
    weak_fill: Color32,
    stroke: Color32,
    fg: Color32,
    stroke_width: f32,
) {
    visuals.bg_fill = fill;
    visuals.weak_bg_fill = weak_fill;
    visuals.bg_stroke = Stroke::new(stroke_width, stroke);
    visuals.fg_stroke = Stroke::new(1.0, fg);
    visuals.corner_radius = CornerRadius::same(12);
    visuals.expansion = 0.0;
}

pub fn apply(ctx: &egui::Context, palette: Palette) {
    let mode = if palette.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(mode);
    let mut style = (*ctx.style_of(mode)).clone();
    let mut visuals = if palette.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let hover_fill = if palette.dark {
        mix(palette.raised, palette.accent, 0.28)
    } else {
        mix(palette.raised, palette.accent, 0.16)
    };
    let active_fill = if palette.dark {
        mix(palette.raised, palette.accent, 0.40)
    } else {
        mix(palette.raised, palette.accent, 0.24)
    };
    let open_fill = mix(
        palette.raised,
        palette.accent,
        if palette.dark { 0.22 } else { 0.14 },
    );
    let selection_fill = mix(
        palette.surface,
        palette.accent,
        if palette.dark { 0.40 } else { 0.28 },
    );
    let shadow_alpha = if palette.dark { 72 } else { 28 };

    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.surface;
    visuals.faint_bg_color = palette.surface;
    visuals.extreme_bg_color = palette.raised;
    visuals.text_edit_bg_color = Some(palette.surface);
    visuals.code_bg_color = palette.raised;
    visuals.hyperlink_color = palette.accent;
    visuals.selection.bg_fill = selection_fill;
    visuals.selection.stroke = Stroke::new(1.0, palette.text);
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(12);
    visuals.window_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 3],
        blur: 10,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    visuals.collapsing_header_frame = true;
    visuals.indent_has_left_vline = false;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    visuals.text_cursor.stroke = Stroke::new(2.0, palette.accent);

    paint_widget(
        &mut visuals.widgets.noninteractive,
        palette.surface,
        palette.surface,
        palette.border,
        palette.muted,
        1.0,
    );
    paint_widget(
        &mut visuals.widgets.inactive,
        palette.raised,
        palette.raised,
        palette.border,
        palette.text,
        1.0,
    );
    paint_widget(
        &mut visuals.widgets.hovered,
        hover_fill,
        hover_fill,
        palette.accent,
        palette.text,
        1.5,
    );
    paint_widget(
        &mut visuals.widgets.active,
        active_fill,
        active_fill,
        palette.accent,
        palette.text,
        1.5,
    );
    paint_widget(
        &mut visuals.widgets.open,
        open_fill,
        open_fill,
        palette.accent,
        palette.text,
        1.5,
    );

    style.visuals = visuals;
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(16.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(19.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(18.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(28.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(18.0));
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.button_padding = egui::vec2(16.0, 12.0);
    style.spacing.window_margin = egui::Margin::same(12);
    style.spacing.menu_margin = egui::Margin::same(10);
    style.spacing.interact_size = egui::vec2(48.0, 48.0);
    style.spacing.icon_width = 20.0;
    style.spacing.icon_width_inner = 12.0;
    style.spacing.icon_spacing = 8.0;
    style.spacing.indent = 18.0;
    style.spacing.combo_width = 148.0;
    style.spacing.extra_text_line_spacing = 2.0;
    style.spacing.scroll.bar_width = 10.0;
    style.spacing.scroll.handle_min_length = 48.0;
    ctx.set_style_of(mode, style);
}
