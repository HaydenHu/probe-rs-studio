//! DSH-desktop inspired visual language.
//!
//! The colours follow the DeepSeek Harness desktop shell: a dedicated
//! navigation rail, a slightly tinted sidebar, a clean centre surface and a
//! blue accent (#4D6BFE). Two complete palettes are provided; the rail has
//! its own colours so it follows the active theme instead of staying dark.

use egui::{Color32, Context, Visuals};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThemeMode {
    Light,
    Dark,
}

impl Default for ThemeMode {
    fn default() -> Self {
        Self::Dark
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub rail: Color32,
    pub rail_icon: Color32,
    pub rail_hover: Color32,
    pub rail_border: Color32,
    pub sidebar: Color32,
    pub bg: Color32,
    pub card: Color32,
    pub card_alt: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub text3: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub console: Color32,
    pub hover: Color32,
}

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub fn palette(mode: ThemeMode) -> Palette {
    match mode {
        ThemeMode::Light => Palette {
            // Light shell: the rail is a cool light grey, slightly deeper than
            // the sidebar, with dark ink icons. It must never stay near-black.
            rail: rgb(0xe9, 0xec, 0xf2),
            rail_icon: rgb(0x5b, 0x62, 0x6e),
            rail_hover: rgb(0xd8, 0xde, 0xea),
            rail_border: rgb(0xd8, 0xdd, 0xe6),
            sidebar: rgb(0xf5, 0xf6, 0xf8),
            bg: rgb(0xff, 0xff, 0xff),
            card: rgb(0xff, 0xff, 0xff),
            card_alt: rgb(0xf7, 0xf8, 0xfa),
            border: rgb(0xe6, 0xe8, 0xec),
            text: rgb(0x0f, 0x11, 0x15),
            text2: rgb(0x61, 0x66, 0x6b),
            text3: rgb(0x81, 0x85, 0x8c),
            accent: rgb(0x4d, 0x6b, 0xfe),
            accent_soft: rgb(0xea, 0xee, 0xff),
            success: rgb(0x0f, 0xa9, 0x6e),
            warn: rgb(0xd9, 0x8a, 0x0b),
            error: rgb(0xe0, 0x3e, 0x48),
            console: rgb(0xf7, 0xf8, 0xfa),
            hover: rgb(0xed, 0xef, 0xf3),
        },
        ThemeMode::Dark => Palette {
            rail: rgb(0x10, 0x11, 0x14),
            rail_icon: rgb(0x9a, 0xa1, 0xab),
            rail_hover: rgb(0x2a, 0x2d, 0x34),
            rail_border: rgb(0x26, 0x28, 0x2e),
            sidebar: rgb(0x17, 0x18, 0x1c),
            bg: rgb(0x10, 0x11, 0x15),
            card: rgb(0x1b, 0x1d, 0x22),
            card_alt: rgb(0x22, 0x24, 0x2a),
            border: Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 0x12),
            text: rgb(0xf9, 0xfa, 0xfb),
            text2: rgb(0xcf, 0xd3, 0xd6),
            text3: rgb(0xad, 0xb2, 0xb8),
            accent: rgb(0x5d, 0x79, 0xff),
            accent_soft: rgb(0x25, 0x2e, 0x50),
            success: rgb(0x35, 0xc8, 0x8e),
            warn: rgb(0xf0, 0xb4, 0x4c),
            error: rgb(0xf2, 0x63, 0x6b),
            console: rgb(0x0c, 0x0d, 0x10),
            hover: rgb(0x2a, 0x2d, 0x34),
        },
    }
}

/// Apply a DSH-like `Visuals` to the egui context.
pub fn apply(ctx: &Context, mode: ThemeMode) {
    let p = palette(mode);
    let mut v = match mode {
        ThemeMode::Light => Visuals::light(),
        ThemeMode::Dark => Visuals::dark(),
    };
    v.override_text_color = Some(p.text);
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.extreme_bg_color = p.console;
    v.faint_bg_color = p.card_alt;
    v.code_bg_color = p.card_alt;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;

    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.weak_bg_fill = p.card;
    v.widgets.noninteractive.fg_stroke.color = p.text2;
    v.widgets.inactive.bg_fill = p.card_alt;
    v.widgets.inactive.weak_bg_fill = p.card_alt;
    v.widgets.inactive.fg_stroke.color = p.text2;
    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.fg_stroke.color = p.text;
    v.widgets.active.bg_fill = p.accent_soft;
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.fg_stroke.color = p.accent;
    v.widgets.open.bg_fill = p.hover;
    v.widgets.open.weak_bg_fill = p.hover;
    v.widgets.open.fg_stroke.color = p.text;
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke.color = p.accent;
    ctx.set_visuals(v);
}

/// egui's built-in fonts contain no CJK glyphs. Append a Windows system font
/// as a fallback so Chinese labels never render as tofu boxes.
pub fn install_fonts(ctx: &Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/NotoSansSC-VF.ttf",
    ];
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = "system-cjk".to_owned();
        fonts
            .font_data
            .insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push(name.clone());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name);
        break;
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Perceived brightness of a colour, 0 (black) .. 255 (white).
    fn luma(c: Color32) -> u32 {
        (c.r() as u32 * 30 + c.g() as u32 * 59 + c.b() as u32 * 11) / 100
    }

    #[test]
    fn rail_and_icons_follow_the_theme() {
        let light = palette(ThemeMode::Light);
        let dark = palette(ThemeMode::Dark);

        // Regression guard: the light rail used to be hard-coded to #151517,
        // which looked identical to the dark rail when switching themes.
        assert!(luma(light.rail) > 180, "light rail must be a light surface");
        assert!(luma(dark.rail) < 60, "dark rail must be a dark surface");
        assert!(luma(light.rail) - luma(dark.rail) > 100);

        // Icons must stay legible on their respective rail backgrounds.
        assert!(luma(light.rail_icon) < 140, "light rail needs dark ink icons");
        assert!(luma(dark.rail_icon) > 120, "dark rail needs light ink icons");
        assert!(luma(light.rail) - luma(light.rail_icon) > 60);
        assert!(luma(dark.rail_icon) - luma(dark.rail) > 60);
    }
}
