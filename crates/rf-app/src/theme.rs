//! smoothyfiles palette: Windows 11 Fluent surfaces + SwiftUI-like softness
//! (rounded corners, gentle fills, calm secondary text).

use gpui_kit::*;

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Palette {
    pub dark: bool,
    /// Window background drawn over Mica (semi-transparent so the backdrop shows through).
    pub window_bg: Hsla,
    pub titlebar_bg: Hsla,
    pub sidebar_bg: Hsla,
    pub content_bg: Hsla,
    /// Opaque surface for menus, dialogs and inputs that float over other content.
    pub card_bg: Hsla,
    /// Cards that sit on the content layer (Home tiles, preview boxes): Fluent's translucent
    /// CardBackgroundFillColorDefault, so the backdrop tint shows through like in Explorer.
    pub tile_bg: Hsla,
    pub tile_hover: Hsla,
    pub border: Hsla,
    pub divider: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub selection: Hsla,
    pub selection_border: Hsla,
    pub tab_active: Hsla,
    pub danger: Hsla,
}

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

fn hexa(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

/// The user's Windows accent palette, read once.
fn system_accent() -> Option<rf_win::accent::AccentPalette> {
    static P: std::sync::OnceLock<Option<rf_win::accent::AccentPalette>> = std::sync::OnceLock::new();
    *P.get_or_init(rf_win::accent::palette)
}

impl Palette {
    // Fluent (Windows 11) values: chrome is Mica, content sits on one translucent layer,
    // selection is a quiet neutral fill plus an accent pill (see `selection_pill`), and the
    // accent is the user's own Windows accent color.
    pub fn light() -> Self {
        let accent = system_accent().map(|a| a.dark1()).unwrap_or(0x005FB8);
        Self {
            dark: false,
            window_bg: hexa(0xF3F3F3, 0.30),
            titlebar_bg: hexa(0xEBEBEB, 0.0),
            sidebar_bg: hexa(0xFFFFFF, 0.0),
            content_bg: hexa(0xFFFFFF, 0.50),
            card_bg: hex(0xF9F9F9),
            tile_bg: hexa(0xFFFFFF, 0.70),
            tile_hover: hexa(0xF9F9F9, 0.50),
            border: hexa(0x000000, 0.07),
            divider: hexa(0x000000, 0.07),
            text: hexa(0x000000, 0.90),
            text_secondary: hexa(0x000000, 0.62),
            text_tertiary: hexa(0x000000, 0.45),
            accent: hex(accent),
            accent_text: hex(0xFFFFFF),
            hover: hexa(0x000000, 0.04),
            pressed: hexa(0x000000, 0.07),
            selection: hexa(0x000000, 0.06),
            selection_border: hexa(accent, 0.55),
            tab_active: hexa(0xFFFFFF, 0.70),
            danger: hex(0xC42B1C),
        }
    }

    pub fn dark() -> Self {
        let accent = system_accent().map(|a| a.light2()).unwrap_or(0x60CDFF);
        Self {
            dark: true,
            // a neutral wash over Mica: keeps its depth but tames a strongly colored wallpaper
            // (a red desktop made the whole window look red)
            window_bg: hexa(0x202020, 0.40),
            titlebar_bg: hexa(0x1C1C1C, 0.0),
            sidebar_bg: hexa(0x000000, 0.0),
            // darker than Fluent's LayerFill (#3A3A3A 30%), close to Explorer's content pane
            // (~#1B1B1B over Mica): calmer, less desktop tint behind the files
            content_bg: hexa(0x141414, 0.45),
            card_bg: hex(0x2C2C2C),
            tile_bg: hexa(0xFFFFFF, 0.0512),
            tile_hover: hexa(0xFFFFFF, 0.0837),
            border: hexa(0xFFFFFF, 0.08),
            divider: hexa(0xFFFFFF, 0.07),
            text: hexa(0xFFFFFF, 0.94),
            text_secondary: hexa(0xFFFFFF, 0.70),
            text_tertiary: hexa(0xFFFFFF, 0.46),
            accent: hex(accent),
            // Fluent: text on a light accent fill is dark
            accent_text: hexa(0x000000, 0.90),
            hover: hexa(0xFFFFFF, 0.06),
            pressed: hexa(0xFFFFFF, 0.10),
            selection: hexa(0xFFFFFF, 0.085),
            selection_border: hexa(accent, 0.55),
            tab_active: hexa(0xFFFFFF, 0.06),
            danger: hex(0xFF99A4),
        }
    }

    pub fn for_window(window: &Window) -> Self {
        let mut p = match crate::settings::theme_override() {
            crate::settings::ThemePref::Light => Self::light(),
            crate::settings::ThemePref::Dark => Self::dark(),
            crate::settings::ThemePref::System => match window.appearance() {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
                _ => Self::light(),
            },
        };
        if crate::settings::solid_backdrop() {
            // "Solid" material: Windows 11's plain window color instead of Mica
            p.window_bg = if p.dark { hex(0x202020) } else { hex(0xF3F3F3) };
        }
        p
    }
}

/// Icon tint per file category (SwiftUI-style colored symbols).
pub fn kind_color(cat: crate::icons::Category, dark: bool) -> Hsla {
    use crate::icons::Category::*;
    let c = match cat {
        Folder => 0xF2B33D,
        Image => 0xAF52DE,
        Video => 0xFF375F,
        Audio => 0xFF9F0A,
        Archive => 0xA2845E,
        Code => 0x30B0C7,
        Document => 0x0A84FF,
        Spreadsheet => 0x30D158,
        Pdf => 0xFF453A,
        Executable => 0x8E8E93,
        Text => 0x8E8E93,
        Other => 0x8E8E93,
    };
    let mut h = hex(c);
    if !dark && matches!(cat, Folder) {
        h.l *= 0.92;
    }
    h
}

/// The selection marker shared by every list (sidebar, details, columns, menus): a short
/// rounded accent bar on the left edge of a selected row. The row must be `relative()`.
pub fn selection_pill(p: &Palette) -> Div {
    div().absolute().left(px(0.)).top(px(8.)).bottom(px(8.)).w(px(3.)).rounded_full().bg(p.accent)
}

pub const RADIUS: Pixels = px(8.);
pub const RADIUS_SM: Pixels = px(6.);
pub const ROW_H: Pixels = px(30.);
pub const TITLEBAR_H: Pixels = px(40.);
