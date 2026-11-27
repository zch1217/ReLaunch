//! Visual theme tokens, egui styling, and custom vector icons.

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Stroke, Vec2, Visuals,
};
use egui::epaint::{PathStroke, Shadow};

/// Color palette for light and dark themes.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg_canvas: Color32,
    pub bg_header: Color32,
    pub bg_card: Color32,
    pub bg_card_hover: Color32,
    pub bg_input: Color32,
    pub bg_input_focus: Color32,
    pub border: Color32,
    pub border_hover: Color32,
    pub divider: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub accent_soft_border: Color32,
    pub accent_soft_text: Color32,
    pub green_dot: Color32,
    pub green_text: Color32,
    pub green_soft: Color32,
    pub stopped_dot: Color32,
    pub star_active: Color32,
    pub star_idle: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub icon_btn_hover: Color32,
    pub msix_bg: Color32,
    pub msix_fg: Color32,
    pub win32_bg: Color32,
    pub win32_fg: Color32,
}

impl Palette {
    pub fn for_mode(dark: bool) -> Self {
        if dark {
            Self::dark()
        } else {
            Self::light()
        }
    }

    pub fn light() -> Self {
        Self {
            bg_canvas: Color32::from_rgb(0xF6, 0xF7, 0xF9),
            bg_header: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            bg_card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            bg_card_hover: Color32::from_rgb(0xF8, 0xFA, 0xFD),
            bg_input: Color32::from_rgb(0xF1, 0xF3, 0xF7),
            bg_input_focus: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border: Color32::from_rgb(0xE5, 0xE7, 0xEB),
            border_hover: Color32::from_rgb(0xCB, 0xD5, 0xE1),
            divider: Color32::from_rgb(0xEA, 0xED, 0xF2),
            text_primary: Color32::from_rgb(0x11, 0x18, 0x27),
            text_secondary: Color32::from_rgb(0x64, 0x74, 0x8B),
            text_muted: Color32::from_rgb(0x94, 0xA3, 0xB8),
            accent: Color32::from_rgb(0x25, 0x63, 0xEB),
            accent_hover: Color32::from_rgb(0x1D, 0x4E, 0xD8),
            accent_soft: Color32::from_rgb(0xEF, 0xF6, 0xFF),
            accent_soft_border: Color32::from_rgb(0xBF, 0xDB, 0xFE),
            accent_soft_text: Color32::from_rgb(0x1D, 0x4E, 0xD8),
            green_dot: Color32::from_rgb(0x10, 0xB9, 0x81),
            green_text: Color32::from_rgb(0x05, 0x96, 0x69),
            green_soft: Color32::from_rgb(0xEC, 0xFD, 0xF5),
            stopped_dot: Color32::from_rgb(0xCB, 0xD5, 0xE1),
            star_active: Color32::from_rgb(0xF5, 0x9E, 0x0B),
            star_idle: Color32::from_rgb(0xCB, 0xD5, 0xE1),
            danger: Color32::from_rgb(0xDC, 0x26, 0x26),
            danger_soft: Color32::from_rgb(0xFE, 0xF2, 0xF2),
            icon_btn_hover: Color32::from_rgb(0xEE, 0xF2, 0xF7),
            msix_bg: Color32::from_rgb(0xEF, 0xF6, 0xFF),
            msix_fg: Color32::from_rgb(0x25, 0x63, 0xEB),
            win32_bg: Color32::from_rgb(0xF1, 0xF5, 0xF9),
            win32_fg: Color32::from_rgb(0x47, 0x55, 0x69),
        }
    }

    pub fn dark() -> Self {
        Self {
            bg_canvas: Color32::from_rgb(0x0F, 0x11, 0x17),
            bg_header: Color32::from_rgb(0x15, 0x18, 0x22),
            bg_card: Color32::from_rgb(0x18, 0x1C, 0x27),
            bg_card_hover: Color32::from_rgb(0x1F, 0x24, 0x33),
            bg_input: Color32::from_rgb(0x1C, 0x20, 0x2D),
            bg_input_focus: Color32::from_rgb(0x22, 0x27, 0x37),
            border: Color32::from_rgb(0x26, 0x2B, 0x3A),
            border_hover: Color32::from_rgb(0x3B, 0x43, 0x5B),
            divider: Color32::from_rgb(0x22, 0x27, 0x35),
            text_primary: Color32::from_rgb(0xF1, 0xF5, 0xF9),
            text_secondary: Color32::from_rgb(0x94, 0xA3, 0xB8),
            text_muted: Color32::from_rgb(0x64, 0x74, 0x8B),
            accent: Color32::from_rgb(0x3B, 0x82, 0xF6),
            accent_hover: Color32::from_rgb(0x25, 0x63, 0xEB),
            accent_soft: Color32::from_rgb(23, 37, 84),
            accent_soft_border: Color32::from_rgb(0x1E, 0x3A, 0x8A),
            accent_soft_text: Color32::from_rgb(0x93, 0xC5, 0xFD),
            green_dot: Color32::from_rgb(0x34, 0xD3, 0x99),
            green_text: Color32::from_rgb(0x34, 0xD3, 0x99),
            green_soft: Color32::from_rgba_unmultiplied(16, 185, 129, 32),
            stopped_dot: Color32::from_rgb(0x47, 0x55, 0x69),
            star_active: Color32::from_rgb(0xFB, 0xBF, 0x24),
            star_idle: Color32::from_rgb(0x47, 0x55, 0x69),
            danger: Color32::from_rgb(0xF8, 0x71, 0x71),
            danger_soft: Color32::from_rgba_unmultiplied(239, 68, 68, 28),
            icon_btn_hover: Color32::from_rgb(0x25, 0x2B, 0x3B),
            msix_bg: Color32::from_rgba_unmultiplied(59, 130, 246, 36),
            msix_fg: Color32::from_rgb(0x93, 0xC5, 0xFD),
            win32_bg: Color32::from_rgb(0x24, 0x29, 0x38),
            win32_fg: Color32::from_rgb(0x94, 0xA3, 0xB8),
        }
    }
}

/// Apply theme visuals, font hinting, and spacing to the egui context.
pub fn apply_visuals(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });

    let p = Palette::for_mode(dark);
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    visuals.text_options.font_hinting = true;
    visuals.panel_fill = p.bg_canvas;
    visuals.window_fill = p.bg_card;
    visuals.extreme_bg_color = p.bg_input;
    visuals.faint_bg_color = p.bg_card_hover;
    visuals.override_text_color = Some(p.text_primary);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.window_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 70 } else { 24 }),
    };
    visuals.popup_shadow = Shadow {
        offset: [0, 2],
        blur: 8,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 60 } else { 20 }),
    };
    visuals.selection.bg_fill = if dark {
        Color32::from_rgb(0x1E, 0x3A, 0x8A)
    } else {
        Color32::from_rgb(0xBF, 0xDB, 0xFE)
    };
    visuals.selection.stroke = Stroke::new(1.0, p.accent);

    for (w, bg, stroke_c, radius) in [
        (&mut visuals.widgets.noninteractive, p.bg_card, p.border, 8u8),
        (&mut visuals.widgets.inactive, p.bg_card, p.border, 8u8),
        (&mut visuals.widgets.hovered, p.bg_card_hover, p.border_hover, 8u8),
        (&mut visuals.widgets.active, p.bg_input_focus, p.accent, 8u8),
        (&mut visuals.widgets.open, p.bg_card_hover, p.border_hover, 8u8),
    ] {
        w.bg_fill = bg;
        w.weak_bg_fill = bg;
        w.bg_stroke = Stroke::new(1.0, stroke_c);
        w.fg_stroke = Stroke::new(1.0, p.text_primary);
        w.corner_radius = CornerRadius::same(radius);
    }

    ctx.global_style_mut(|style| {
        style.visuals = visuals;
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.scroll.bar_width = 6.0;
    });
}

/// RichText rendered with the bold font family.
pub fn bold_text(text: impl Into<String>, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(text)
        .font(FontId::new(size, FontFamily::Name("bold".into())))
        .color(color)
}

// ---------------------------------------------------------------------------
// Custom vector icons (painted with egui::Painter)
// ---------------------------------------------------------------------------

pub fn paint_star(painter: &egui::Painter, center: Pos2, radius: f32, filled: bool, color: Color32) {
    let inner_r = radius * 0.43;
    let mut pts = Vec::with_capacity(10);
    for i in 0..10 {
        let angle = -FRAC_PI_2 + (i as f32) * (TAU / 10.0);
        let r = if i % 2 == 0 { radius } else { inner_r };
        pts.push(Pos2::new(
            center.x + angle.cos() * r,
            center.y + angle.sin() * r,
        ));
    }
    if filled {
        for i in 0..10 {
            let tri = vec![center, pts[i], pts[(i + 1) % 10]];
            painter.add(egui::Shape::convex_polygon(tri, color, PathStroke::NONE));
        }
    } else {
        painter.add(egui::Shape::closed_line(pts, Stroke::new(1.4, color)));
    }
}

pub fn paint_refresh(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    angle_rad: f32,
    color: Color32,
) {
    const STEPS: usize = 16;
    let start = -FRAC_PI_2 + angle_rad;
    let sweep = 1.55 * PI;
    let mut arc = Vec::with_capacity(STEPS + 1);
    for i in 0..=STEPS {
        let a = start + sweep * (i as f32 / STEPS as f32);
        arc.push(Pos2::new(center.x + a.cos() * radius, center.y + a.sin() * radius));
    }
    painter.add(egui::Shape::line(arc, Stroke::new(1.7, color)));

    let end_a = start + sweep;
    let end_p = Pos2::new(
        center.x + end_a.cos() * radius,
        center.y + end_a.sin() * radius,
    );
    let radial = Vec2::new(end_a.cos(), end_a.sin());
    let tangent = Vec2::new(-end_a.sin(), end_a.cos());
    let head = radius * 0.42;
    let tip = end_p + tangent * (head * 0.9);
    let p1 = end_p - radial * (head * 0.85) - tangent * (head * 0.25);
    let p2 = end_p + radial * (head * 0.85) - tangent * (head * 0.25);
    painter.add(egui::Shape::convex_polygon(
        vec![tip, p1, p2],
        color,
        PathStroke::NONE,
    ));
}

pub fn paint_plus(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.8, color);
    painter.line_segment(
        [
            Pos2::new(center.x - radius, center.y),
            Pos2::new(center.x + radius, center.y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(center.x, center.y - radius),
            Pos2::new(center.x, center.y + radius),
        ],
        stroke,
    );
}

pub fn paint_search(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let ring_center = Pos2::new(center.x - radius * 0.16, center.y - radius * 0.16);
    let ring_r = radius * 0.62;
    let stroke = Stroke::new(1.5, color);
    painter.circle_stroke(ring_center, ring_r, stroke);
    let handle_start = ring_center + Vec2::splat(ring_r * 0.72);
    let handle_end = center + Vec2::splat(radius * 0.88);
    painter.line_segment([handle_start, handle_end], stroke);
}

pub fn paint_close(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    painter.line_segment(
        [
            Pos2::new(center.x - radius, center.y - radius),
            Pos2::new(center.x + radius, center.y + radius),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(center.x + radius, center.y - radius),
            Pos2::new(center.x - radius, center.y + radius),
        ],
        stroke,
    );
}

pub fn paint_back(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.7, color);
    let left = Pos2::new(center.x - radius * 0.85, center.y);
    let right = Pos2::new(center.x + radius * 0.85, center.y);
    painter.line_segment([left, right], stroke);
    painter.line_segment(
        [
            left,
            Pos2::new(left.x + radius * 0.68, center.y - radius * 0.68),
        ],
        stroke,
    );
    painter.line_segment(
        [
            left,
            Pos2::new(left.x + radius * 0.68, center.y + radius * 0.68),
        ],
        stroke,
    );
}

pub fn paint_copy(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    let front = Rect::from_min_max(
        Pos2::new(center.x - radius * 0.35, center.y - radius * 0.35),
        Pos2::new(center.x + radius * 0.85, center.y + radius * 0.85),
    );
    painter.rect_stroke(front, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);

    // Back sheet offset to the top-left.
    let bx0 = center.x - radius * 0.85;
    let by0 = center.y - radius * 0.85;
    let bx1 = center.x + radius * 0.35;
    let by1 = center.y + radius * 0.35;
    painter.add(egui::Shape::line(
        vec![
            Pos2::new(bx0 + radius * 0.22, by1),
            Pos2::new(bx0, by1),
            Pos2::new(bx0, by0),
            Pos2::new(bx1, by0),
            Pos2::new(bx1, by0 + radius * 0.22),
        ],
        stroke,
    ));
}

pub fn paint_folder(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let stroke = Stroke::new(1.35, color);
    let l = center.x - radius * 0.92;
    let r = center.x + radius * 0.92;
    let t = center.y - radius * 0.72;
    let b = center.y + radius * 0.72;
    let tab_r = center.x - radius * 0.15;
    let shoulder = center.y - radius * 0.42;
    painter.add(egui::Shape::closed_line(
        vec![
            Pos2::new(l, t),
            Pos2::new(tab_r, t),
            Pos2::new(tab_r + radius * 0.24, shoulder),
            Pos2::new(r, shoulder),
            Pos2::new(r, b),
            Pos2::new(l, b),
        ],
        stroke,
    ));
}

pub fn paint_theme_icon(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    dark_mode: bool,
    color: Color32,
    bg_color: Color32,
) {
    if dark_mode {
        painter.circle_filled(center, radius * 0.42, color);
        let stroke = Stroke::new(1.5, color);
        for i in 0..8 {
            let a = (i as f32) * (PI / 4.0);
            let dir = Vec2::new(a.cos(), a.sin());
            painter.line_segment(
                [center + dir * (radius * 0.65), center + dir * (radius * 0.94)],
                stroke,
            );
        }
    } else {
        painter.circle_filled(center, radius * 0.82, color);
        painter.circle_filled(
            center + Vec2::new(radius * 0.36, -radius * 0.28),
            radius * 0.68,
            bg_color,
        );
    }
}

/// Deterministic monogram badge when an app icon is unavailable.
pub fn paint_monogram(painter: &egui::Painter, rect: Rect, name: &str, dark: bool) {
    const LIGHT_TONES: [(Color32, Color32); 6] = [
        (Color32::from_rgb(0xEB, 0xF2, 0xFF), Color32::from_rgb(0x25, 0x63, 0xEB)),
        (Color32::from_rgb(0xEC, 0xFD, 0xF5), Color32::from_rgb(0x05, 0x96, 0x69)),
        (Color32::from_rgb(0xF5, 0xF3, 0xFF), Color32::from_rgb(0x7C, 0x3A, 0xED)),
        (Color32::from_rgb(0xFF, 0xF7, 0xED), Color32::from_rgb(0xD9, 0x77, 0x06)),
        (Color32::from_rgb(0xFC, 0xE7, 0xF3), Color32::from_rgb(0xDB, 0x27, 0x77)),
        (Color32::from_rgb(0xEC, 0xFE, 0xFF), Color32::from_rgb(0x08, 0x91, 0xB2)),
    ];
    const DARK_TONES: [(Color32, Color32); 6] = [
        (Color32::from_rgb(0x1E, 0x29, 0x42), Color32::from_rgb(0x93, 0xC5, 0xFD)),
        (Color32::from_rgb(0x13, 0x30, 0x2A), Color32::from_rgb(0x6E, 0xE7, 0xB7)),
        (Color32::from_rgb(0x28, 0x1E, 0x42), Color32::from_rgb(0xC4, 0xB5, 0xFD)),
        (Color32::from_rgb(0x36, 0x26, 0x16), Color32::from_rgb(0xFD, 0xBA, 0x74)),
        (Color32::from_rgb(0x3A, 0x1D, 0x2E), Color32::from_rgb(0xF9, 0xA8, 0xD4)),
        (Color32::from_rgb(0x15, 0x2E, 0x38), Color32::from_rgb(0x67, 0xE8, 0xF9)),
    ];

    let mut hash: usize = 5381;
    for b in name.to_lowercase().bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as usize);
    }
    let (bg, fg) = if dark {
        DARK_TONES[hash % DARK_TONES.len()]
    } else {
        LIGHT_TONES[hash % LIGHT_TONES.len()]
    };

    painter.rect_filled(rect, CornerRadius::same(8), bg);
    let ch = name
        .chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ch,
        FontId::new((rect.height() * 0.46).max(11.0), FontFamily::Name("bold".into())),
        fg,
    );
}
