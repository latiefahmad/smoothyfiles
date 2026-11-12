//! Metin ve kod görüntüleyici. Dosya parça parça okunur: satır başları arka planda dizinlenir,
//! yalnız ekrandaki satırlar çözülür ve çizilir (çok büyük günlük dosyaları da anında açılır).
//! 2 MB'a kadar bilinen kod dosyaları syntect ile arka planda renklendirilir.
//! Alt+L satır numaraları, Alt+Z sözcük kaydırma (ayar olarak saklanır).

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parking_lot::RwLock;
use rf_core::textload::TextFile;

use crate::look::viewer::{Prepared, Probe, Viewer, looks_like_text};
use crate::look::config::LookSettings;

/// Bilinen metin uzantıları; bunların dışında içerik metne benziyorsa da açılır.
pub const EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "log", "ini", "cfg", "conf", "toml", "yaml", "yml", "json", "jsonc", "json5", "xml", "csv", "tsv", "rs", "c",
    "h", "cpp", "hpp", "cc", "cs", "java", "kt", "go", "py", "rb", "php", "js", "mjs", "cjs", "ts", "tsx", "jsx", "css", "scss", "less",
    "html", "htm", "vue", "svelte", "sh", "bash", "zsh", "ps1", "psm1", "bat", "cmd", "sql", "lua", "pl", "r", "swift", "dart", "gradle",
    "cmake", "make", "mk", "dockerfile", "gitignore", "gitattributes", "editorconfig", "env", "srt", "vtt", "ass", "nfo", "reg", "inf",
    "properties", "lock", "diff", "patch", "tex", "rst", "adoc", "asm", "s", "hlsl", "glsl", "wgsl", "zig", "nim", "ex", "exs", "erl",
    "hs", "ml", "fs", "clj", "scala", "groovy", "vb", "vbs", "tcl", "awk", "sed", "csproj", "sln", "props", "targets", "xaml", "plist",
];

/// Metin sezgisi yalnız bu boyuta kadar denenir (büyük ikili dosyalar metin sanılmasın).
const SNIFF_MAX: u64 = 64 << 20;
/// Renklendirme bu boyuta kadar (tüm dosya arka planda işlenir).
const HIGHLIGHT_MAX: u64 = 2 << 20;
/// Bir satırdan en çok bu kadar karakter çizilir.
const LINE_MAX_CHARS: usize = 20_000;

#[derive(Default)]
pub struct TextViewer {
    view: Option<Entity<TextView>>,
}

impl Viewer for TextViewer {
    fn name(&self) -> &'static str {
        "text"
    }
    fn priority(&self) -> i32 {
        -5
    }
    fn can_handle(&self, f: &Probe) -> bool {
        if !f.is_file() {
            return false;
        }
        if EXTENSIONS.contains(&f.ext.as_str()) {
            return true;
        }
        // certificates get the info card (subject, issuer, validity), even as PEM text
        if ["cer", "crt", "der", "pem"].contains(&f.ext.as_str()) {
            return false;
        }
        let small = std::fs::metadata(&f.path).map_or(true, |m| m.len() <= SNIFF_MAX);
        small && !f.head.is_empty() && looks_like_text(&f.head)
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        let name = super::file_name(&f.path);
        let path = f.path.clone();
        self.view = Some(cx.new(|cx| TextView::new(&path, cx)));
        Prepared::new((900.0, 700.0), name)
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn on_key(&mut self, key: &str, mods: &Modifiers, _window: &mut Window, cx: &mut App) -> bool {
        if !mods.alt || mods.control || mods.shift {
            return false;
        }
        let toggle: fn(&mut LookSettings) = match key {
            "l" => |s| s.look_text_line_numbers = !s.look_text_line_numbers,
            "z" => |s| s.look_text_wrap = !s.look_text_wrap,
            _ => return false,
        };
        crate::look::config::update(cx, |s| toggle(s));
        if let Some(v) = &self.view {
            v.update(cx, |v, cx| v.relayout(cx));
        }
        true
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

/// Satır başına renk aralıkları (bayt aralığı, renk 0xRRGGBB).
type Spans = Vec<Vec<(Range<usize>, u32)>>;

pub struct TextView {
    file: Option<Arc<TextFile>>,
    error: Option<String>,
    list: ListState,
    spans: Arc<RwLock<Option<Spans>>>,
    _tasks: Vec<Task<()>>,
    /// Horizontal scroll (px) when lines don't wrap.
    hoff: f32,
    /// Longest line drawn so far (characters) and the view's width: the horizontal range.
    cols: std::rc::Rc<std::cell::Cell<usize>>,
    view_w: std::rc::Rc<std::cell::Cell<f32>>,
}

fn syntax_highlight(path: &Path, text: &str, dark: bool) -> Option<Spans> {
    use syntect::easy::HighlightLines;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;
    use std::sync::OnceLock;
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    let ss = SS.get_or_init(SyntaxSet::load_defaults_newlines);
    let ts = TS.get_or_init(ThemeSet::load_defaults);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let syntax = ss.find_syntax_by_extension(ext).or_else(|| ss.find_syntax_by_first_line(text.lines().next().unwrap_or("")))?;
    if syntax.name == "Plain Text" {
        return None;
    }
    let theme = &ts.themes[if dark { "base16-ocean.dark" } else { "InspiredGitHub" }];
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(text) {
        let regions = h.highlight_line(line, ss).ok()?;
        let mut pos = 0;
        let mut spans = Vec::new();
        for (style, piece) in regions {
            let c = style.foreground;
            let end = pos + piece.trim_end_matches(['\n', '\r']).len();
            if end > pos {
                spans.push((pos..end, (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32));
            }
            pos += piece.len();
        }
        out.push(spans);
    }
    Some(out)
}

impl TextView {
    fn new(path: &Path, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(400.));
        let mut v = Self { file: None, error: None, list, spans: Default::default(), _tasks: Vec::new(), hoff: 0., cols: Default::default(), view_w: Default::default() };
        match TextFile::open(path) {
            Ok(f) => {
                let f = Arc::new(f);
                v.file = Some(f.clone());
                v.index(f.clone(), cx);
                if f.len() <= HIGHLIGHT_MAX {
                    v.highlight(path, cx);
                }
            }
            Err(e) => v.error = Some(e.to_string()),
        }
        v
    }

    /// Satır başlarını arka planda dizinler; liste büyüdükçe güncellenir.
    fn index(&mut self, f: Arc<TextFile>, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let worker = f.clone();
        std::thread::Builder::new()
            .name("look-textindex".into())
            .spawn(move || {
                let mut last = std::time::Instant::now();
                let mut first = true;
                while worker.index_chunk() {
                    if first || last.elapsed() > Duration::from_millis(120) {
                        first = false;
                        last = std::time::Instant::now();
                        if tx.unbounded_send(()).is_err() {
                            return;
                        }
                    }
                }
                let _ = tx.unbounded_send(());
            })
            .ok();
        self.sync_count();
        self._tasks.push(cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while rx.next().await.is_some() {
                if this
                    .update(cx, |v, cx| {
                        v.sync_count();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn sync_count(&mut self) {
        let Some(f) = &self.file else { return };
        let n = f.line_count();
        let old = self.list.item_count();
        if n > old {
            self.list.splice(old..old, n - old);
        }
    }

    fn highlight(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        let spans = self.spans.clone();
        let dark = cx.window_appearance_dark();
        let task = cx.background_spawn(async move {
            let bytes = std::fs::read(&path).ok()?;
            let (text, _) = rf_core::textload::detect_and_decode(&bytes);
            syntax_highlight(&path, &text, dark)
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            if let Some(s) = task.await {
                *spans.write() = Some(s);
                let _ = this.update(cx, |v, cx| {
                    v.list.remeasure();
                    cx.notify();
                });
            }
        }));
    }

    /// Satır numarası ya da kaydırma değişti: satırlar yeniden ölçülür.
    fn relayout(&mut self, cx: &mut Context<Self>) {
        self.list.remeasure();
        cx.notify();
    }
}

/// Sekmeleri dört boşluğa açar; renk aralıklarını (özgün satırdaki bayt konumları) buna göre kaydırır.
fn expand_tabs(line: &str, spans: &[(Range<usize>, u32)]) -> (String, Vec<(Range<usize>, u32)>) {
    if !line.contains('\t') {
        return (line.to_string(), spans.to_vec());
    }
    let shift = |pos: usize| pos + 3 * line.as_bytes()[..pos.min(line.len())].iter().filter(|b| **b == b'\t').count();
    let spans = spans.iter().map(|(r, c)| (shift(r.start)..shift(r.end), *c)).collect();
    (line.replace('\t', "    "), spans)
}

trait DarkAppearance {
    fn window_appearance_dark(&self) -> bool;
}

impl DarkAppearance for App {
    fn window_appearance_dark(&self) -> bool {
        matches!(self.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark)
    }
}

impl Render for TextView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::for_window(window);
        let s = cx.global::<LookSettings>();
        let (numbers, wrap) = (s.look_text_line_numbers, s.look_text_wrap);
        let font_size = s.look_text_font_size.clamp(6.0, 72.0);
        let font: SharedString = if s.look_text_font.trim().is_empty() { "Cascadia Mono".into() } else { s.look_text_font.clone().into() };
        let Some(file) = self.file.clone() else {
            return div().size_full().p_4().text_color(p.danger).child(self.error.clone().unwrap_or_default()).into_any_element();
        };
        let digits = (file.line_count().max(1) as f64).log10().floor() as usize + 1;
        let gutter = px(font_size * 0.62 * digits.max(3) as f32 + 16.0);
        let spans = self.spans.clone();
        let (text_c, num_c) = (p.text, p.text_tertiary);
        // horizontal range without wrapping: the longest line seen × a monospace advance
        let char_w = font_size * 0.6;
        let total_w = self.cols.get() as f32 * char_w + f32::from(gutter) + 32.0;
        let view_w = self.view_w.get();
        let max_off = if wrap { 0.0 } else { (total_w - view_w).max(0.0) };
        self.hoff = self.hoff.clamp(0.0, max_off);
        let hoff = self.hoff;
        let cols = self.cols.clone();
        let vw = self.view_w.clone();
        div()
            .size_full()
            .relative()
            .bg(p.content_bg)
            // Shift+wheel or a sideways swipe scrolls long lines (vertical scrolling is the list's)
            .on_scroll(cx.listener(move |this, ev: &ScrollEvent, _, cx| {
                if wrap {
                    return;
                }
                let d = ev.delta.pixel_delta(px(font_size * 1.45));
                let sideways = if ev.modifiers.shift { f32::from(d.y) } else { f32::from(d.x) };
                if sideways != 0.0 {
                    this.hoff = (this.hoff - sideways).clamp(0.0, max_off);
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(canvas(move |b, _, _| vw.set(f32::from(b.size.width)), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
            .font_family(font)
            .text_size(px(font_size))
            .line_height(px(font_size * 1.45))
            .child(
                list(self.list.clone(), move |i, _window, _cx| {
                    let mut line = file.line(i).unwrap_or_default();
                    if line.len() > LINE_MAX_CHARS {
                        let mut cut = LINE_MAX_CHARS;
                        while !line.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        line.truncate(cut);
                        line.push('…');
                    }
                    let raw: Vec<(Range<usize>, u32)> = spans.read().as_ref().and_then(|s| s.get(i)).cloned().unwrap_or_default();
                    let (line, raw) = expand_tabs(&line, &raw);
                    let highlights: Vec<(Range<usize>, HighlightStyle)> = raw
                        .into_iter()
                        .filter(|(r, _)| r.end <= line.len() && line.is_char_boundary(r.start) && line.is_char_boundary(r.end))
                        .map(|(r, c)| (r, HighlightStyle { color: Some(rgb(c).into()), ..Default::default() }))
                        .collect();
                    if !wrap && line.len() > cols.get() {
                        cols.set(line.chars().count());
                    }
                    let text = StyledText::new(line).with_highlights(highlights);
                    h_flex()
                        .w_full()
                        .items_start()
                        .px_2()
                        .when(numbers, |d| {
                            d.child(div().w(gutter).flex_shrink_0().pr_3().text_right().text_color(num_c).child((i + 1).to_string()))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(text_c)
                                .map(|d| if wrap { d.whitespace_normal().child(text) } else { d.overflow_hidden().child(div().whitespace_nowrap().ml(px(-hoff)).child(text)) }),
                        )
                        .into_any_element()
                })
                .size_full(),
            )
            // a thin horizontal bar: where the view is along the longest line
            .when(!wrap && max_off > 0.0 && view_w > 0.0, |d| {
                let thumb = (view_w * view_w / total_w).max(24.0);
                let left = (view_w - thumb) * (hoff / max_off);
                d.child(div().absolute().bottom(px(2.)).left(px(left)).w(px(thumb)).h(px(4.)).rounded_full().bg(num_c).opacity(0.7))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::expand_tabs;

    #[test]
    fn spans_follow_expanded_tabs() {
        // "\tx = 1": "x" 1..2 (sekmeden sonra), "1" 5..6
        let (line, spans) = expand_tabs("\tx = 1", &[(1..2, 0xff0000), (5..6, 0x00ff00)]);
        assert_eq!(line, "    x = 1");
        assert_eq!(spans, vec![(4..5, 0xff0000), (8..9, 0x00ff00)]);
        assert_eq!(&line[spans[0].0.clone()], "x");
        assert_eq!(&line[spans[1].0.clone()], "1");
    }
}
