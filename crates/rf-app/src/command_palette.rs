//! Command palette (Ctrl+Shift+P / F1): fuzzy-filter every command, or type a path to jump.

use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::*;
use crate::theme::{self, Palette};

struct Cmd {
    label: &'static str,
    keys: &'static str,
    icon: L,
    action: fn() -> Box<dyn Action>,
}

macro_rules! cmd {
    ($label:expr, $keys:expr, $icon:expr, $act:expr) => {
        Cmd { label: crate::i18n::t($label), keys: $keys, icon: $icon, action: || Box::new($act) }
    };
}

fn commands() -> Vec<Cmd> {
    vec![
        cmd!("Yeni sekme", "Ctrl+T", L::Plus, NewTab),
        cmd!("Yeni pencere", "Ctrl+N", L::AppWindow, NewWindow),
        cmd!("Sekmeyi kapat", "Ctrl+W", L::X, CloseTab),
        cmd!("Kapatılan sekmeyi yeniden aç", "Ctrl+Shift+T", L::RotateCcw, ReopenClosedTab),
        cmd!("Yeni klasör", "Ctrl+Shift+N", L::FolderPlus, NewFolder),
        cmd!("Yeni metin belgesi", "", L::FilePlus, NewTextFile),
        cmd!("Yenile", "F5", L::RotateCw, Refresh),
        cmd!("Ağ sürücüsüne bağlan…", "", L::Network, MapNetworkDrive),
        cmd!("Ağ sürücüsü bağlantısını kes…", "", L::Network, DisconnectNetworkDrive),
        cmd!("Geri", "Alt+←", L::ArrowLeft, GoBack),
        cmd!("İleri", "Alt+→", L::ArrowRight, GoForward),
        cmd!("Üst klasör", "Alt+↑", L::ArrowUp, GoUp),
        cmd!("Ana sayfa", "Alt+Home", L::House, GoHome),
        cmd!("Ayarlar", "Ctrl+,", L::Settings, GoSettings),
        cmd!("Geri dönüşüm kutusu", "", L::Trash, GoRecycleBin),
        cmd!("Adres çubuğuna git", "Ctrl+L", L::TextCursorInput, FocusAddress),
        cmd!("Ara", "Ctrl+F", L::Search, FocusSearch),
        cmd!("Gizli öğeleri göster/gizle", "Ctrl+H", L::Eye, ToggleHidden),
        cmd!("Detay görünümü", "Ctrl+Shift+1", L::List, ViewDetails),
        cmd!("Izgara görünümü", "Ctrl+Shift+2", L::LayoutGrid, ViewGrid),
        cmd!("Sütun görünümü", "Ctrl+Shift+3", L::Columns3, ViewColumns),
        cmd!("Kart görünümü", "Ctrl+Shift+4", L::LayoutList, ViewCards),
        cmd!("Liste görünümü", "Ctrl+Shift+5", L::TextAlignJustify, ViewList),
        cmd!("Simgeleri büyüt", "Ctrl+=", L::ZoomIn, ZoomIn),
        cmd!("Rafa ekle", "Ctrl+Shift+S", L::Inbox, AddToShelf),
        cmd!("Rafı göster/gizle", "", L::Inbox, ToggleShelf),
        cmd!("Tam ekran", "F11", L::Maximize, ToggleFullscreen),
        cmd!("Her zaman üstte", "", L::Pin, ToggleAlwaysOnTop),
        cmd!("Kompakt görünüm", "", L::PictureInPicture2, ToggleCompactOverlay),
        cmd!("Klasör takma adına git", "@", L::AtSign, GoToFolderAlias),
        cmd!("Sağa döndür", "", L::RotateCw, RotateRight),
        cmd!("Sola döndür", "", L::RotateCcw, RotateLeft),
        cmd!("Simgeleri küçült", "Ctrl+-", L::ZoomOut, ZoomOut),
        cmd!("Çift panel aç/kapat", "Ctrl+Shift+D", L::Columns2, ToggleDualPane),
        cmd!("Panel değiştir", "F6", L::ArrowLeftRight, SwitchPane),
        cmd!("Önizleme paneli", "Alt+P", L::PanelRight, TogglePreview),
        cmd!("Kenar çubuğu", "Ctrl+B", L::PanelLeft, ToggleSidebar),
        cmd!("Terminalde aç", "Ctrl+Shift+`", L::SquareTerminal, OpenTerminal),
        cmd!("Tümünü seç", "Ctrl+A", L::SquareCheck, SelectAll),
        cmd!("Seçimi tersine çevir", "", L::ArrowLeftRight, InvertSelection),
        cmd!("Öğeyi seç/bırak", "Ctrl+Space", L::SquareCheck, ToggleSelection),
        cmd!("Kopyala", "Ctrl+C", L::Copy, CopyItems),
        cmd!("Kes", "Ctrl+X", L::Scissors, CutItems),
        cmd!("Yapıştır", "Ctrl+V", L::ClipboardPaste, PasteItems),
        cmd!("Seçili klasöre yapıştır", "Ctrl+Shift+V", L::ClipboardPaste, PasteIntoSelected),
        cmd!("Kısayol olarak yapıştır", "", L::Link, PasteShortcut),
        cmd!("Geri dönüşüm kutusuna taşı", "Del", L::Trash, DeleteItems),
        cmd!("Kalıcı olarak sil", "Shift+Del", L::TrashOff, DeletePermanently),
        cmd!("Yeniden adlandır", "F2", L::Pencil, RenameItem),
        cmd!("Geri al", "Ctrl+Z", L::Undo2, Undo),
        cmd!("Yolu kopyala", "Ctrl+Shift+C", L::Link, CopyPath),
        cmd!("Birlikte aç…", "", L::AppWindow, OpenWith),
        cmd!("Özellikler", "Alt+Enter", L::Info, Properties),
        cmd!("Sağ tık menüsü", "Shift+F10", L::Menu, ShowContextMenu),
    ]
}

/// (label, shortcut) for every command that has one (shown in Settings).
/// Every command for the shortcut editor: (label, default keys, action short name).
pub fn shortcut_entries() -> Vec<(&'static str, &'static str, String)> {
    commands()
        .into_iter()
        .map(|c| {
            let name = (c.action)().name().rsplit("::").next().unwrap_or_default().to_string();
            (c.label, c.keys, name)
        })
        .collect()
}

#[allow(dead_code)]
pub fn shortcut_list() -> Vec<(&'static str, &'static str)> {
    commands().into_iter().filter(|c| !c.keys.is_empty()).map(|c| (c.label, c.keys)).collect()
}

pub enum PaletteEvent {
    Dismiss,
    Run(Box<dyn Action>),
    Navigate(PathBuf),
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

pub struct CommandPalette {
    input: Entity<InputState>,
    cmds: Vec<Cmd>,
    /// Indices into `cmds` matching the query (plus an optional path row at the top).
    filtered: Vec<usize>,
    path: Option<PathBuf>,
    selected: usize,
    _sub: Subscription,
}

fn score(label: &str, q: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let l = label.to_lowercase();
    if let Some(i) = l.find(q) {
        return Some(100 - i as i32);
    }
    // subsequence match ("ykl" → "yeni klasör")
    let mut it = l.chars();
    let mut gaps = 0;
    for qc in q.chars() {
        let mut found = false;
        for c in it.by_ref() {
            if c == qc {
                found = true;
                break;
            }
            gaps += 1;
        }
        if !found {
            return None;
        }
    }
    Some(50 - gaps)
}

fn as_path(q: &str) -> Option<PathBuf> {
    let t = q.trim().trim_matches('"');
    let looks = t.len() >= 2 && (t.as_bytes()[1] == b':' || t.starts_with("\\\\") || t.starts_with('%'));
    looks.then(|| PathBuf::from(t))
}

impl CommandPalette {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(crate::t!("Komut ara veya yol yaz…")));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => this.refilter(cx),
            InputEvent::PressEnter { .. } => this.run_selected(cx),
            _ => {}
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        let mut this = Self { input, cmds: commands(), filtered: Vec::new(), path: None, selected: 0, _sub: sub };
        this.refilter(cx);
        this
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.input.read(cx).value().to_lowercase();
        self.path = as_path(&q).map(|_| PathBuf::from(self.input.read(cx).value().trim().trim_matches('"')));
        let mut v: Vec<(i32, usize)> =
            self.cmds.iter().enumerate().filter_map(|(i, c)| score(c.label, q.trim()).map(|s| (s, i))).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.filtered = v.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        cx.notify();
    }

    fn rows(&self) -> usize {
        self.filtered.len() + usize::from(self.path.is_some())
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        let mut ix = self.selected;
        if let Some(p) = &self.path {
            if ix == 0 {
                cx.emit(PaletteEvent::Navigate(p.clone()));
                return;
            }
            ix -= 1;
        }
        if let Some(&ci) = self.filtered.get(ix) {
            cx.emit(PaletteEvent::Run((self.cmds[ci].action)()));
        }
    }

    fn row(&self, ix: usize, icon: L, label: String, keys: &'static str, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let sel = ix == self.selected;
        h_flex()
            .id(("pal", ix))
            .h(px(34.))
            .px_3()
            .gap_3()
            .items_center()
            .rounded(theme::RADIUS_SM)
            .text_sm()
            .when(sel, |d| d.bg(p.selection))
            .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
            .child(Icon::new(icon).size(px(16.)).text_color(if sel { p.accent } else { p.text_secondary }))
            .child(div().flex_1().child(label))
            .when(!keys.is_empty(), |d| d.child(div().text_xs().text_color(p.text_tertiary).child(keys)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = ix;
                this.run_selected(cx);
            }))
            .into_any_element()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let mut list = v_flex().gap_0p5();
        let mut ix = 0;
        if let Some(path) = self.path.clone() {
            list = list.child(self.row(ix, L::FolderOpen, format!("{} {}", crate::t!("Git:"), path.display()), "Enter", &p, cx));
            ix += 1;
        }
        let visible: Vec<usize> = self.filtered.iter().copied().take(12).collect();
        for ci in visible {
            let c = &self.cmds[ci];
            let (icon, label, keys) = (c.icon, c.label.to_string(), c.keys);
            list = list.child(self.row(ix, icon, label, keys, &p, cx));
            ix += 1;
        }
        // scrim + card
        div()
            .id("palette-scrim")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(hsla(0., 0., 0., 0.25))
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .child(
                v_flex()
                    .id("palette")
                    .absolute()
                    .top(px(72.))
                    .left(relative(0.5))
                    .ml(px(-290.))
                    .w(px(580.))
                    .p_2()
                    .gap_2()
                    .rounded(theme::RADIUS)
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                        let n = this.rows();
                        match ev.keystroke.key.as_str() {
                            "down" if n > 0 => {
                                this.selected = (this.selected + 1) % n.min(13);
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "up" if n > 0 => {
                                this.selected = (this.selected + n.min(13) - 1) % n.min(13);
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "escape" => {
                                cx.stop_propagation();
                                cx.emit(PaletteEvent::Dismiss);
                            }
                            _ => {}
                        }
                    }))
                    .child(
                        div()
                            .h(px(38.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(theme::RADIUS_SM)
                            .bg(p.hover)
                            .child(
                                Input::new(&self.input)
                                    .appearance(false)
                                    .prefix(Icon::new(L::Command).size(px(14.)).text_color(p.text_secondary)),
                            ),
                    )
                    .child(list)
                    .when(self.rows() == 0, |d| {
                        d.child(div().p_3().text_sm().text_color(p.text_secondary).child(crate::t!("Eşleşen komut yok")))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{as_path, score};
    use std::path::PathBuf;

    #[test]
    fn fuzzy() {
        assert!(score("Yeni klasör", "klas").unwrap() > score("Yeni klasör", "ykl").unwrap());
        assert!(score("Yeni klasör", "xyz").is_none());
        assert_eq!(as_path(r"C:\Windows"), Some(PathBuf::from(r"C:\Windows")));
        assert!(as_path("yeni").is_none());
    }
}
