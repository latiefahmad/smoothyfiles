//! Arşiv görüntüleyici: içeriği ağaç olarak (klasörler açılır/kapanır), özet satırı
//! ("ZIP arşivi, N klasör ve M dosya"), sıkıştırılmış/açılmış boyut; "smoothyfiles'ta aç" Diğer'de.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::format;

use crate::look::viewer::{MoreItem, Prepared, Probe, Viewer};
use crate::t;
use crate::theme::Palette;

#[derive(Default)]
pub struct ArchiveViewer {
    view: Option<Entity<ArchiveView>>,
    path: PathBuf,
}

impl Viewer for ArchiveViewer {
    fn name(&self) -> &'static str {
        "archive"
    }
    fn priority(&self) -> i32 {
        -5
    }
    fn can_handle(&self, f: &Probe) -> bool {
        // app packages (APK, MSIX, IPA, NuGet) are zips too, but what they are says more: the info card
        f.is_file() && rf_archive::is_supported(&f.path) && rf_win::apppkg::kind_of(&f.path).is_none()
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        self.path = f.path.clone();
        let path = f.path.clone();
        self.view = Some(cx.new(|cx| ArchiveView::new(path, cx)));
        let mut p = Prepared::new((760.0, 560.0), super::file_name(&f.path));
        p.drag_by_body = false;
        p
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        self.view.as_ref().map_or_else(|| div().into_any_element(), |v| v.clone().into_any_element())
    }
    fn more_menu(&self) -> Vec<MoreItem> {
        // DirectLook ayrı süreç: çıkarma smoothyfiles'ın işlem merkezinde yapılır, arşiv oraya gönderilir.
        let path = self.path.clone();
        vec![MoreItem {
            label: t!("smoothyfiles'ta aç").into(),
            action: Rc::new(move |_, cx: &mut App| {
                crate::look::LookManager::close(cx);
                open_in_smoothyfiles(path.clone());
            }),
        }]
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

/// smoothyfiles'a gidecek ileti ve (çalışmıyorsa) başlatma argümanı: klasör gibi gezilebilen arşiv
/// kendisi açılır, ötekiler (.gz gibi) klasöründe seçili gösterilir.
pub fn smoothyfiles_launch(path: &Path) -> (String, String) {
    let p = path.display().to_string();
    if crate::archive_view::is_browsable(path) {
        (p.clone(), p)
    } else {
        (format!("{}{p}", crate::workspace::SELECT_PREFIX), format!("/select,{p}"))
    }
}

fn open_in_smoothyfiles(path: PathBuf) {
    std::thread::spawn(move || {
        let (msg, arg) = smoothyfiles_launch(&path);
        rf_win::look::win::allow_any_foreground();
        if rf_win::single_instance::send_to("smoothyfiles", &msg) {
            return;
        }
        let exe = std::env::current_exe().map(|e| e.with_file_name("smoothyfiles.exe"));
        match exe {
            Ok(exe) if exe.exists() => {
                if let Err(e) = std::process::Command::new(exe).arg(arg).spawn() {
                    log::warn!("smoothyfiles başlatılamadı: {e}");
                }
            }
            _ => log::warn!("smoothyfiles.exe bulunamadı"),
        }
    });
}

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    /// Arşiv içindeki yol (`/` ile).
    pub key: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_ms: i64,
    pub children: Vec<Node>,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub root: Node,
    pub folders: usize,
    pub files: usize,
    pub size: u64,
    pub packed: u64,
}

#[derive(Default)]
struct Building {
    is_dir: bool,
    size: u64,
    modified_ms: i64,
    children: BTreeMap<String, Building>,
}

fn finish(name: String, key: String, b: Building) -> Node {
    let mut children: Vec<Node> = b
        .children
        .into_iter()
        .map(|(n, c)| {
            let k = if key.is_empty() { n.clone() } else { format!("{key}/{n}") };
            finish(n, k, c)
        })
        .collect();
    children.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| rf_core::sort::natural_cmp(&a.name, &b.name)));
    let size = if b.is_dir { children.iter().map(|c| c.size).sum() } else { b.size };
    Node { name, key, is_dir: b.is_dir, size, modified_ms: b.modified_ms, children }
}

/// Düz girdi listesinden ağaç (ara klasörler kendiliğinden oluşur).
pub fn build_tree(entries: &[rf_archive::ArchiveEntry]) -> Tree {
    let mut root = Building { is_dir: true, ..Default::default() };
    let (mut folders, mut files, mut size, mut packed) = (0, 0, 0u64, 0u64);
    for e in entries {
        let norm = e.path.replace('\\', "/");
        let parts: Vec<&str> = norm.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        if parts.is_empty() {
            continue;
        }
        let mut cur = &mut root;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            let dir = !last || e.is_dir;
            let is_new = !cur.children.contains_key(*part);
            let node = cur.children.entry((*part).to_string()).or_default();
            if is_new {
                if dir {
                    folders += 1;
                } else {
                    files += 1;
                }
            }
            if dir {
                node.is_dir = true;
            } else {
                node.size = e.size;
                node.modified_ms = e.modified_ms;
            }
            cur = node;
        }
        if !e.is_dir {
            size += e.size;
            packed += e.packed;
        }
    }
    Tree { root: finish(String::new(), String::new(), root), folders, files, size, packed }
}

pub struct Row<'a> {
    pub depth: usize,
    pub key: String,
    pub node: &'a Node,
}

/// Görünen satırlar: açık klasörlerin içi.
pub fn rows<'a>(root: &'a Node, open: &HashSet<String>) -> Vec<Row<'a>> {
    fn walk<'a>(n: &'a Node, depth: usize, open: &HashSet<String>, out: &mut Vec<Row<'a>>) {
        for c in &n.children {
            out.push(Row { depth, key: c.key.clone(), node: c });
            if c.is_dir && open.contains(&c.key) {
                walk(c, depth + 1, open, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, open, &mut out);
    out
}

pub struct ArchiveView {
    path: PathBuf,
    tree: Option<Tree>,
    error: Option<String>,
    open: HashSet<String>,
    /// The shown rows (built when the tree or the open folders change, not every frame).
    rows: Option<Rc<Vec<ShownRow>>>,
    scroll: UniformListScrollHandle,
    _task: Task<()>,
}

/// (depth, key, name, is a folder, size, date, open, icon)
type ShownRow = (usize, String, String, bool, u64, i64, bool, L);

impl ArchiveView {
    fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let p = path.clone();
        let opts = crate::archive_view::options(&path);
        // the tree is built off the UI thread too (hundreds of thousands of entries take a while)
        let task = cx.background_spawn(async move { rf_archive::list_with(&p, &opts).map(|list| build_tree(&list)) });
        let task = cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                v.rows = None;
                match r {
                    Ok(tree) => {
                        // tek kök klasörlü arşivde o klasör açık gelir
                        if tree.root.children.len() == 1 && tree.root.children[0].is_dir {
                            v.open.insert(tree.root.children[0].key.clone());
                        }
                        v.tree = Some(tree);
                    }
                    Err(e) => {
                        v.error = Some(if crate::archive_view::password_problem(&e).is_some() {
                            t!("Bu arşiv parolalı; içeriğini görmek için klasör olarak açın").to_string()
                        } else {
                            format!("{e:#}")
                        })
                    }
                }
                cx.notify();
            });
        });
        Self { path, tree: None, error: None, open: HashSet::new(), rows: None, scroll: UniformListScrollHandle::new(), _task: task }
    }

    fn kind_label(&self) -> String {
        let name = self.path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let ext = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            "TAR.GZ".to_string()
        } else {
            self.path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_uppercase()
        };
        format!("{ext} {}", t!("arşivi"))
    }
}

fn icon_for(n: &Node) -> L {
    if n.is_dir {
        return L::Folder;
    }
    let ext = n.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "heic" | "avif" | "tif" | "tiff" => L::FileImage,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "mts" | "m2ts" | "mxf" => L::FileVideoCamera,
        "mp3" | "flac" | "wav" | "m4a" | "ogg" | "opus" | "aac" => L::FileMusic,
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "zst" | "bz2" | "cab" | "iso" => L::FileArchive,
        "rs" | "c" | "cpp" | "h" | "py" | "js" | "ts" | "json" | "xml" | "html" | "css" | "toml" | "yaml" | "yml" => L::FileCode,
        "txt" | "md" | "log" | "pdf" | "doc" | "docx" | "rtf" => L::FileText,
        "xls" | "xlsx" | "csv" => L::FileSpreadsheet,
        _ => L::File,
    }
}

const ROW_H: f32 = 26.0;

impl Render for ArchiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let Some(tree) = &self.tree else {
            let msg = self.error.clone().unwrap_or_else(|| t!("Yükleniyor…").to_string());
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(if self.error.is_some() { p.danger } else { p.text_secondary })
                .child(msg)
                .into_any_element();
        };
        let summary = format!(
            "{}, {} {} {} {} {}",
            self.kind_label(),
            tree.folders,
            t!("klasör"),
            t!("ve"),
            tree.files,
            t!("dosya")
        );
        let sizes = format!(
            "{} {} · {} {}",
            t!("Sıkıştırılmış:"),
            if tree.packed > 0 { format::size(tree.packed) } else { "—".into() },
            t!("Açılmış:"),
            format::size(tree.size)
        );
        let list = match &self.rows {
            Some(r) => r.clone(),
            None => {
                let built: Vec<ShownRow> = rows(&tree.root, &self.open)
                    .into_iter()
                    .map(|r| {
                        let open = self.open.contains(&r.key);
                        (r.depth, r.key, r.node.name.clone(), r.node.is_dir, r.node.size, r.node.modified_ms, open, icon_for(r.node))
                    })
                    .collect();
                let built = Rc::new(built);
                self.rows = Some(built.clone());
                built
            }
        };
        let n = list.len();
        let view = cx.entity().downgrade();
        v_flex()
            .size_full()
            .bg(p.content_bg)
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap_0p5()
                    .border_b_1()
                    .border_color(p.divider)
                    .child(div().text_sm().text_color(p.text).child(summary))
                    .child(div().text_xs().text_color(p.text_secondary).child(sizes)),
            )
            .child(
                uniform_list("look-archive", n, move |range, _window, _cx| {
                    range
                        .map(|i| {
                            let (depth, key, name, is_dir, size, ms, open, icon) = list[i].clone();
                            let view = view.clone();
                            h_flex()
                                .id(i)
                                .w_full()
                                .h(px(ROW_H))
                                .px_3()
                                .gap_1p5()
                                .items_center()
                                .text_sm()
                                .hover(|s| s.bg(p.hover))
                                .child(div().w(px(depth as f32 * 16.0)).flex_shrink_0())
                                .child(
                                    div()
                                        .w(px(14.))
                                        .flex_shrink_0()
                                        .when(is_dir, |d| d.child(Icon::new(if open { L::ChevronDown } else { L::ChevronRight }).size(px(12.)))),
                                )
                                .child(Icon::new(icon).size(px(16.)).text_color(if is_dir { rgb(0xE8B931).into() } else { p.text_secondary }))
                                .child(div().flex_1().min_w_0().truncate().text_color(p.text).child(name))
                                .child(div().w(px(90.)).flex_shrink_0().text_right().text_color(p.text_secondary).child(if is_dir {
                                    String::new()
                                } else {
                                    format::size(size)
                                }))
                                .child(div().w(px(130.)).flex_shrink_0().text_right().text_xs().text_color(p.text_tertiary).child(if ms > 0 {
                                    format::datetime(ms)
                                } else {
                                    String::new()
                                }))
                                .when(is_dir, |d| {
                                    d.on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |v, cx| {
                                            if !v.open.remove(&key) {
                                                v.open.insert(key.clone());
                                            }
                                            v.rows = None;
                                            cx.notify();
                                        });
                                    })
                                })
                        })
                        .collect()
                })
                .track_scroll(&self.scroll)
                .flex_1(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{build_tree, rows, smoothyfiles_launch};

    #[test]
    fn archives_open_in_smoothyfiles_as_folder_or_selected() {
        let zip = std::path::Path::new(r"C:\a b\x.zip");
        assert_eq!(smoothyfiles_launch(zip), (r"C:\a b\x.zip".to_string(), r"C:\a b\x.zip".to_string()));
        let gz = std::path::Path::new(r"C:\a\log.gz");
        let (msg, arg) = smoothyfiles_launch(gz);
        assert_eq!(msg, format!("{}C:\\a\\log.gz", crate::workspace::SELECT_PREFIX));
        assert_eq!(arg, r"/select,C:\a\log.gz");
        // ileti smoothyfiles tarafında doğru çözülür
        assert_eq!(crate::workspace::parse_launch(&msg).1.as_deref(), Some("log.gz"));
    }

    fn e(path: &str, is_dir: bool, size: u64) -> rf_archive::ArchiveEntry {
        rf_archive::ArchiveEntry { path: path.into(), size, packed: size / 2, is_dir, modified_ms: 0, encrypted: false }
    }

    #[test]
    fn builds_tree_from_flat_list() {
        let t = build_tree(&[e("a/b/c.txt", false, 10), e("a/d.txt", false, 5), e("e/", true, 0)]);
        assert_eq!(t.folders, 3);
        assert_eq!(t.files, 2);
        assert_eq!(t.root.children.len(), 2);
        assert_eq!((t.size, t.packed), (15, 7));
    }

    #[test]
    fn folders_first_then_names() {
        let t = build_tree(&[e("z.txt", false, 1), e("b/x", false, 1), e("a.txt", false, 1)]);
        let names: Vec<_> = t.root.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["b", "a.txt", "z.txt"]);
    }

    #[test]
    fn rows_follow_expansion() {
        let t = build_tree(&[e("a/b/c.txt", false, 10), e("a/d.txt", false, 5), e("e/", true, 0)]);
        let mut open = std::collections::HashSet::new();
        assert_eq!(rows(&t.root, &open).len(), 2);
        open.insert("a".to_string());
        let r = rows(&t.root, &open);
        assert_eq!(r.iter().map(|r| (r.depth, r.key.as_str())).collect::<Vec<_>>(), [(0, "a"), (1, "a/b"), (1, "a/d.txt"), (0, "e")]);
    }

    #[test]
    fn backslashes_and_dot_segments() {
        let t = build_tree(&[e("a\\b.txt", false, 1), e("./c.txt", false, 1)]);
        assert_eq!(t.folders, 1);
        assert_eq!(t.files, 2);
    }
}
