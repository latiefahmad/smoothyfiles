//! Font files (TTF, OTF, TTC): the family drawn at several sizes with a pangram, Turkish
//! letters and digits, plus its name, style and glyph count. The font is loaded into this
//! process only (not installed).

use std::borrow::Cow;

use gpui_kit::component::v_flex;
use gpui_kit::*;

use crate::look::viewer::{Prepared, Probe, Viewer};

const EXTENSIONS: &[&str] = &["ttf", "otf", "ttc"];
const SIZES: &[f32] = &[12.0, 18.0, 24.0, 36.0, 48.0, 72.0];

#[derive(Default)]
pub struct FontViewer {
    info: Option<FontInfo>,
    error: Option<String>,
}

struct FontInfo {
    family: SharedString,
    style: String,
    glyphs: u16,
}

/// Family and style names (English where the font has several), and the glyph count.
fn read_info(data: &[u8]) -> Option<FontInfo> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let name = |ids: &[u16]| {
        for id in ids {
            let mut fallback = None;
            for n in face.names().into_iter().filter(|n| n.name_id == *id) {
                if let Some(s) = n.to_string() {
                    if n.language_id == 0x0409 {
                        return Some(s);
                    }
                    fallback.get_or_insert(s);
                }
            }
            if fallback.is_some() {
                return fallback;
            }
        }
        None
    };
    let family = name(&[ttf_parser::name_id::TYPOGRAPHIC_FAMILY, ttf_parser::name_id::FAMILY])?;
    let style = name(&[ttf_parser::name_id::TYPOGRAPHIC_SUBFAMILY, ttf_parser::name_id::SUBFAMILY]).unwrap_or_default();
    Some(FontInfo { family: family.into(), style, glyphs: face.number_of_glyphs() })
}

impl Viewer for FontViewer {
    fn name(&self) -> &'static str {
        "font"
    }
    fn priority(&self) -> i32 {
        -4
    }
    fn can_handle(&self, f: &Probe) -> bool {
        f.is_file() && EXTENSIONS.contains(&f.ext.as_str())
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        match std::fs::read(&f.path) {
            Ok(data) => match read_info(&data) {
                Some(info) => {
                    if let Err(e) = cx.text_system().add_fonts(vec![Cow::Owned(data)]) {
                        self.error = Some(e.to_string());
                    } else {
                        self.info = Some(info);
                    }
                }
                None => self.error = Some(crate::t!("Yazı tipi okunamadı").to_string()),
            },
            Err(e) => self.error = Some(e.to_string()),
        }
        Prepared::new((900.0, 760.0), super::file_name(&f.path))
    }
    fn render(&mut self, window: &mut Window, _cx: &mut App) -> AnyElement {
        let p = crate::theme::Palette::for_window(window);
        let Some(info) = &self.info else {
            return div().size_full().p_4().text_color(p.danger).child(self.error.clone().unwrap_or_default()).into_any_element();
        };
        let mut col = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(info.family.clone()))
                    .child(div().text_sm().text_color(p.text_secondary).child(format!("{} · {} {}", info.style, info.glyphs, crate::t!("glif")))),
            )
            .child(
                div()
                    .font_family(info.family.clone())
                    .text_size(px(28.))
                    .child("AaBbCcÇçDdEeFfGgĞğHhIıİiJjKkLlMmNnOoÖöPpRrSsŞşTtUuÜüVvYyZz 0123456789"),
            );
        for size in SIZES {
            col = col.child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_3()
                    .child(div().w(px(36.)).flex_shrink_0().text_xs().text_color(p.text_tertiary).child(format!("{size}")))
                    .child(
                        div()
                            .font_family(info.family.clone())
                            .text_size(px(*size))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child("Pijamalı hasta yağız şoföre çabucak güvendi. The quick brown fox jumps over the lazy dog."),
                    ),
            );
        }
        div()
            .id("dl-font")
            .size_full()
            .overflow_y_scroll()
            .bg(p.content_bg)
            .text_color(p.text)
            .p_8()
            .child(col)
            .into_any_element()
    }
    fn cleanup(&mut self) {
        self.info = None;
        self.error = None;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_a_system_font() {
        let path = std::path::Path::new(r"C:\Windows\Fonts\segoeui.ttf");
        let Ok(data) = std::fs::read(path) else { return };
        let info = super::read_info(&data).expect("Segoe UI parses");
        assert_eq!(&*info.family, "Segoe UI");
        assert!(info.glyphs > 1000);
    }
}
