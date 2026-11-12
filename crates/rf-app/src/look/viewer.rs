//! Görüntüleyici sözleşmesi: dosyaya bakış (`Probe`), öncelik sırasıyla seçim (`pick`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::{AnyElement, App, Modifiers, SharedString, Window};

/// Dosyanın başından okunan bayt sayısı (tür tanıma için).
pub const HEAD_LEN: usize = 16 * 1024;

/// Görüntüleyicilerin karar verdiği bilgi: ad, uzantı, ilk baytlar.
#[derive(Clone, Debug)]
pub struct Probe {
    pub path: PathBuf,
    /// Küçük harf, noktasız.
    pub ext: String,
    /// İlk 16 KB (klasörde, yoksa ya da okunamazsa boş).
    pub head: Vec<u8>,
    pub is_dir: bool,
    pub exists: bool,
}

impl Probe {
    pub fn ext_of(p: &Path) -> String {
        p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
    }

    pub fn read(path: &Path) -> Probe {
        let virtual_item = path.to_string_lossy().starts_with("::");
        let meta = if virtual_item { None } else { std::fs::metadata(path).ok() };
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let mut head = Vec::new();
        if meta.as_ref().is_some_and(|m| m.is_file()) {
            if let Ok(f) = std::fs::File::open(path) {
                let _ = f.take(HEAD_LEN as u64).read_to_end(&mut head);
            }
        }
        Probe { path: path.to_path_buf(), ext: Probe::ext_of(path), head, is_dir, exists: meta.is_some() }
    }

    /// Normal, var olan bir dosya (klasör ya da sanal öğe değil).
    pub fn is_file(&self) -> bool {
        self.exists && !self.is_dir
    }
}

/// Whether the first bytes look like text: no run of two NULs, and at most 5 % control bytes
/// (tab, line breaks, form feed and escape don't count). UTF-16 (with a BOM, or with every other
/// byte zero) is text even though half its bytes are NUL.
pub fn looks_like_text(head: &[u8]) -> bool {
    if head.windows(2).any(|w| w == [0, 0]) {
        return false;
    }
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    let n = head.len();
    if n >= 8 {
        let zeros_at = |start: usize| head.iter().skip(start).step_by(2).filter(|&&b| b == 0).count();
        let half = n / 2;
        if zeros_at(1) * 10 >= half * 7 || zeros_at(0) * 10 >= half * 7 {
            return true; // UTF-16 without a BOM
        }
    }
    let control = head.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b)).count();
    control * 20 <= n
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Dark,
    Light,
}

/// What a viewer's [`Viewer::loader`] opened off the UI thread, handed back through
/// [`Viewer::take_loaded`].
pub type Loaded = Box<dyn std::any::Any + Send>;

/// Görüntüleyicinin pencereye bildirdikleri.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// İçeriğin istediği boyut (mantıksal piksel).
    pub preferred: (f32, f32),
    /// Çalışma alanının en çok bu oranı kadar büyür.
    pub fit_ratio: f32,
    /// Başlık çubuğundaki metin (ör. "1920×1080: manzara.jpg").
    pub title: String,
    pub theme: Option<ThemeChoice>,
    /// Başlık çubuğu içeriğin üstünde durur ve kendiliğinden gizlenir (resim, video).
    pub overlay_titlebar: bool,
    pub resizable: bool,
    /// Gövdeden sürükleme dosyayı pencereden dışarı sürükler.
    pub drag_by_body: bool,
}

impl Prepared {
    pub fn new(preferred: (f32, f32), title: impl Into<String>) -> Prepared {
        Prepared { preferred, fit_ratio: 0.8, title: title.into(), theme: None, overlay_titlebar: false, resizable: true, drag_by_body: true }
    }
}

/// "Diğer" menüsüne görüntüleyicinin eklediği öğe.
#[derive(Clone)]
pub struct MoreItem {
    pub label: SharedString,
    pub action: Rc<dyn Fn(&mut Window, &mut App)>,
}

pub trait Viewer: 'static {
    fn name(&self) -> &'static str;
    fn priority(&self) -> i32;
    fn can_handle(&self, f: &Probe) -> bool;
    /// The slow part of opening a file (a container's index, a PDF's pages). For a file on a
    /// network drive or another slow place it runs off the UI thread and its result reaches
    /// [`Viewer::take_loaded`] before [`Viewer::prepare`]; elsewhere `prepare` does the work
    /// itself. Viewers that only start background work in `prepare` need none.
    fn loader(&self) -> Option<fn(&Probe) -> Loaded> {
        None
    }
    fn take_loaded(&mut self, _loaded: Loaded) {}
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared;
    fn render(&mut self, window: &mut Window, cx: &mut App) -> AnyElement;
    fn more_menu(&self) -> Vec<MoreItem> {
        vec![]
    }
    /// Pencerenin işlemediği (değiştiricili) tuşlar; işlendiyse true.
    fn on_key(&mut self, _key: &str, _mods: &Modifiers, _window: &mut Window, _cx: &mut App) -> bool {
        false
    }
    fn cleanup(&mut self) {}
}

/// Öncelik sırasıyla ilk işleyebilen görüntüleyici (eşit öncelikte kayıt sırası); hiçbiri yoksa Bilgi kartı.
pub fn pick(probe: &Probe) -> Box<dyn Viewer> {
    let mut all = super::viewers::all();
    all.sort_by_key(|v| std::cmp::Reverse(v.priority()));
    all.into_iter().find(|v| v.can_handle(probe)).unwrap_or_else(|| Box::new(super::viewers::info::InfoCard::default()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn probe(name: &str, head: &[u8]) -> Probe {
        Probe { path: PathBuf::from(name), ext: Probe::ext_of(Path::new(name)), head: head.to_vec(), is_dir: false, exists: true }
    }

    #[test]
    fn text_heuristic() {
        assert!(looks_like_text(b"merhaba\ndunya"));
        assert!(!looks_like_text(b"ab\0\0cd"));
        // binary with single NULs and control bytes; UTF-16 with and without a BOM; tabs and ANSI
        let bin: Vec<u8> = (0u8..=255).collect();
        assert!(!looks_like_text(&bin));
        assert!(looks_like_text(&[0xFF, 0xFE, b'a', 0, b'b', 0]));
        assert!(looks_like_text(&[b'm', 0, b'e', 0, b'r', 0, b'h', 0, b'a', 0, b'b', 0]));
        assert!(looks_like_text(b"col1\tcol2\r\nx\ty\r\n\x1b[31mred\x1b[0m"));
    }

    #[test]
    fn picks_by_priority() {
        assert_eq!(pick(&probe("a.svg", b"<svg")).name(), "image");
        assert_eq!(pick(&probe("a.zip", b"PK\x03\x04")).name(), "archive");
        assert_eq!(pick(&probe("a.unknownext", b"\0\0\x01")).name(), "hex");
        assert_eq!(pick(&probe("setup.exe", b"MZ\x90\0\x03\0\0\0")).name(), "info");
        assert_eq!(pick(&probe("notlar.unknownext", b"metin")).name(), "text");
        assert_eq!(pick(&probe("klip.mp4", b"\0\0\0\x18ftypmp42")).name(), "video");
        assert_eq!(pick(&probe("rapor.pdf", b"%PDF-1.7\n")).name(), "pdf");
        // a PDF with the wrong extension is still a PDF
        assert_eq!(pick(&probe("rapor.dat", b"%PDF-1.4\n")).name(), "pdf");
        // design files: the picture saved inside them; an Illustrator file that is a PDF is a PDF
        assert_eq!(pick(&probe("afis.cdr", b"PK\x03\x04\x14\0\0\x08")).name(), "image");
        assert_eq!(pick(&probe("logo.ai", b"%PDF-1.6\n%")).name(), "pdf");
        assert_eq!(pick(&probe("logo.eps", b"\xC5\xD0\xD3\xC6\x20\0\0\0")).name(), "image");
        // a macOS "._" sidecar named like a design is not one
        assert_eq!(pick(&probe("._afis.cdr", b"\0\x05\x16\x07\0\x02\0\0")).name(), "hex");
    }

    /// `.ts` hem TypeScript hem MPEG-TS: içerik karar verir (188 baytlık paketlerde 0x47 eşitleme baytı).
    #[test]
    fn ts_is_typescript_unless_transport_stream() {
        assert_eq!(pick(&probe("app.ts", b"export const x: number = 1;\n")).name(), "text");
        let mut ts = vec![0u8; 400];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        assert_eq!(pick(&probe("kayit.ts", &ts)).name(), "video");
    }

    #[test]
    fn missing_file_goes_to_info() {
        assert_eq!(pick(&Probe::read(Path::new(r"C:\yok\x.mp4"))).name(), "info");
    }

    #[test]
    fn virtual_item_goes_to_info() {
        assert_eq!(pick(&Probe::read(Path::new("::{645FF040-5081-101B-9F08-00AA002F954E}"))).name(), "info");
    }

    #[test]
    fn folder_goes_to_info() {
        assert_eq!(pick(&Probe::read(&std::env::temp_dir())).name(), "info");
    }
}
