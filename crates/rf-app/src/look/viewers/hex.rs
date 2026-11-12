//! Binary files no other viewer understands: the file's name and size, then a hex dump of its
//! first 256 KB (offset, 16 bytes, their ASCII), in a virtual list. Programs and certificates
//! keep their info cards; text goes to the text viewer.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

use crate::look::viewer::{Prepared, Probe, Viewer, looks_like_text};
use crate::t;

const SHOW_MAX: usize = 256 << 10;
const ROW: usize = 16;
/// Files the info card describes better (version resource, certificate fields).
const INFO_CARD: &[&str] = &["exe", "dll", "sys", "ocx", "cpl", "scr", "drv", "efi", "mui", "ax", "cer", "crt", "der", "pem"];

#[derive(Default)]
pub struct HexViewer {
    view: Option<Entity<HexView>>,
}

impl Viewer for HexViewer {
    fn name(&self) -> &'static str {
        "hex"
    }
    fn priority(&self) -> i32 {
        // after every viewer that understands the format, before the plain info card
        -50
    }
    fn can_handle(&self, f: &Probe) -> bool {
        // programs, certificates, app packages and ELF files get the info card (what they are)
        f.is_file()
            && !f.head.is_empty()
            && !looks_like_text(&f.head)
            && !INFO_CARD.contains(&f.ext.as_str())
            && rf_win::apppkg::kind_of(&f.path).is_none()
            && !f.head.starts_with(b"ELF")
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        let path = f.path.clone();
        self.view = Some(cx.new(|_| HexView::new(&path)));
        Prepared::new((980.0, 680.0), super::file_name(&f.path))
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

pub struct HexView {
    bytes: Arc<Vec<u8>>,
    total: u64,
    error: Option<String>,
    scroll: UniformListScrollHandle,
}

fn read_head(path: &Path) -> std::io::Result<(Vec<u8>, u64)> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    let mut buf = Vec::with_capacity(SHOW_MAX.min(total as usize));
    f.take(SHOW_MAX as u64).read_to_end(&mut buf)?;
    Ok((buf, total))
}

/// One row: "00000010" offset, the bytes in two groups of eight, their printable ASCII.
pub(crate) fn row_text(bytes: &[u8], row: usize) -> (String, String, String) {
    let start = row * ROW;
    let chunk = &bytes[start.min(bytes.len())..(start + ROW).min(bytes.len())];
    let mut hex = String::with_capacity(ROW * 3 + 1);
    for i in 0..ROW {
        if i == 8 {
            hex.push(' ');
        }
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02X} ")),
            None => hex.push_str("   "),
        }
    }
    let ascii: String = chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '·' }).collect();
    (format!("{start:08X}"), hex.trim_end().to_string(), ascii)
}

impl HexView {
    fn new(path: &Path) -> Self {
        let (bytes, total, error) = match read_head(path) {
            Ok((b, t)) => (b, t, None),
            Err(e) => (Vec::new(), 0, Some(e.to_string())),
        };
        Self { bytes: Arc::new(bytes), total, error, scroll: UniformListScrollHandle::new() }
    }
}

impl Render for HexView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::for_window(window);
        if let Some(e) = &self.error {
            return div().size_full().p_4().text_color(p.danger).child(e.clone()).into_any_element();
        }
        let rows = self.bytes.len().div_ceil(ROW);
        let partial = (self.bytes.len() as u64) < self.total;
        let note = if partial {
            format!("{} · {}", rf_core::format::size(self.total), t!("İlk 256 KB gösteriliyor"))
        } else {
            rf_core::format::size(self.total)
        };
        let mono = || div().font_family("Cascadia Mono").text_size(px(13.));
        let (text_c, dim_c, faint_c) = (p.text, p.text_secondary, p.text_tertiary);
        let head = h_flex()
            .px_4()
            .h(px(30.))
            .flex_shrink_0()
            .gap_6()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(mono().w(px(76.)).text_color(faint_c).child(t!("Ofset")))
            .child(mono().w(px(420.)).text_color(faint_c).child("00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F"))
            .child(mono().text_color(faint_c).child("ASCII"));
        let bytes = self.bytes.clone();
        v_flex()
            .size_full()
            .bg(p.content_bg)
            .child(h_flex().px_4().py_2().gap_3().items_center().text_sm().text_color(dim_c).child(note))
            .child(head)
            .child(
                uniform_list("hex-rows", rows, cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
                    range
                        .map(|r| {
                            let (off, hex, ascii) = row_text(&bytes, r);
                            h_flex()
                                .px_4()
                                .h(px(22.))
                                .gap_6()
                                .items_center()
                                .child(mono().w(px(76.)).text_color(faint_c).child(off))
                                .child(mono().w(px(420.)).text_color(text_c).child(hex))
                                .child(mono().text_color(dim_c).child(ascii))
                                .into_any_element()
                        })
                        .collect()
                }))
                .track_scroll(&self.scroll)
                .flex_1(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_have_offset_hex_and_ascii() {
        let data: Vec<u8> = (0u8..20).chain(b"Hi!".iter().copied()).collect();
        let (off, hex, ascii) = super::row_text(&data, 0);
        assert_eq!(off, "00000000");
        assert_eq!(hex, "00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F");
        assert_eq!(ascii.chars().count(), 16);
        let (off, hex, ascii) = super::row_text(&data, 1);
        assert_eq!(off, "00000010");
        assert_eq!(hex, "10 11 12 13 48 69 21");
        assert_eq!(ascii, "····Hi!");
    }
}
