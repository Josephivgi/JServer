//! Color, type and spacing tokens. Layout follows the JAgents SSH page;
//! colors follow JPerformance. Every color in the UI comes from here.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Theme, Visuals};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Tokens {
    /// Page background (JAgents --canvas).
    pub canvas: Color32,
    /// Cards and the sidebar (--surface).
    pub surface: Color32,
    /// Meter tracks and hovered rows (--well).
    pub well: Color32,
    /// Hairlines (--line).
    pub line: Color32,
    pub ink: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_ink: Color32,
    pub accent_soft: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// Neutral grays and the blue accent from JPerformance, so the two apps match.
pub const LIGHT: Tokens = Tokens {
    canvas: hex(0xf3f3f1),
    surface: hex(0xfcfcfb),
    well: hex(0xefeeeb),
    line: hex(0xe0dfdb),
    ink: hex(0x0b0b0b),
    muted: hex(0x6b6a65),
    accent: hex(0x2a78d6),
    accent_ink: hex(0xffffff),
    accent_soft: hex(0xe3eefb),
    ok: hex(0x008300),
    warn: hex(0xb57a00),
    danger: hex(0xd23c3b),
};

pub const DARK: Tokens = Tokens {
    canvas: hex(0x121211),
    surface: hex(0x1a1a19),
    well: hex(0x242422),
    line: hex(0x2f2f2c),
    ink: hex(0xf0efec),
    muted: hex(0x908f88),
    accent: hex(0x3987e5),
    accent_ink: hex(0xffffff),
    accent_soft: hex(0x1f2a38),
    ok: hex(0x2fa82f),
    warn: hex(0xe0a020),
    danger: hex(0xe66767),
};

pub fn tokens(ctx: &egui::Context) -> Tokens {
    match ctx.theme() {
        Theme::Dark => DARK,
        Theme::Light => LIGHT,
    }
}

/// --radius in JAgents.
pub const RADIUS: u8 = 8;
/// Cards and tables use 12px corners.
pub const CARD_RADIUS: u8 = 12;

/// The semibold face, for titles (CSS font-weight 600).
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, bold())
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::default();
    f.font_data.insert("plex".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/IBMPlexSans-400.ttf"))));
    f.font_data.insert("plex-600".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/IBMPlexSans-600.ttf"))));
    // JAgents' mono stack starts with Cascadia; it ships with Windows 11.
    for (name, file) in [("cascadia", "CascadiaMono.ttf"), ("consolas", "consola.ttf")] {
        let path = std::path::Path::new(r"C:\Windows\Fonts").join(file);
        if let Ok(bytes) = std::fs::read(path) {
            f.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            f.families.entry(FontFamily::Monospace).or_default().insert(0, name.into());
            break;
        }
    }
    // The Plex files only cover Latin; egui's own fonts fill in symbols.
    let fallback = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    f.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "plex".into());
    let mut bold = vec!["plex-600".to_string()];
    bold.extend(fallback);
    f.families.insert(self::bold(), bold);
    f
}

fn visuals(t: &Tokens, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = t.canvas;
    v.window_fill = t.surface;
    v.extreme_bg_color = t.surface;
    v.faint_bg_color = t.well;
    v.window_stroke = Stroke::new(1.0, t.line);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.override_text_color = Some(t.ink);
    v.hyperlink_color = t.accent;
    v.selection.bg_fill = t.accent_soft;
    v.selection.stroke = Stroke::new(1.0, t.accent);
    let r = CornerRadius::same(RADIUS);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.muted);
    v.widgets.inactive.bg_fill = t.surface;
    v.widgets.inactive.weak_bg_fill = t.surface;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, t.line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, t.ink);
    v.widgets.hovered.bg_fill = t.well;
    v.widgets.hovered.weak_bg_fill = t.well;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, t.line);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, t.ink);
    v.widgets.active.bg_fill = t.well;
    v.widgets.active.weak_bg_fill = t.well;
    v.widgets.active.bg_stroke = Stroke::new(1.0, t.muted);
    v.widgets.active.fg_stroke = Stroke::new(1.0, t.ink);
    v.widgets.open = v.widgets.active;
    v
}

pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.interaction.selectable_labels = false;
        use egui::TextStyle;
        s.text_styles.insert(TextStyle::Heading, bold_font(15.0));
        s.text_styles.insert(TextStyle::Body, font(13.5));
        s.text_styles.insert(TextStyle::Button, font(13.0));
        s.text_styles.insert(TextStyle::Small, font(12.0));
        s.text_styles.insert(TextStyle::Monospace, mono(12.0));
    });
}

/// `.ssh-card`: surface fill, hairline border, 12px corners, no shadow.
pub fn card(t: &Tokens) -> egui::Frame {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.line))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(egui::Margin::symmetric(14, 12))
}

/// Meter color by how full it is, like `.ssh-bar i` (75% warn, 90% danger).
pub fn level(t: &Tokens, pct: f64) -> Color32 {
    if pct >= 90.0 {
        t.danger
    } else if pct >= 75.0 {
        t.warn
    } else {
        t.ok
    }
}
