//! File operations, clipboard, inline rename and the native context menu for a FolderView.
//! Everything that can block runs on worker threads or the shell STA thread.

use crate::t;
use std::path::PathBuf;

use futures::channel::oneshot;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use rf_win::ctxmenu::MenuOutcome;
use rf_win::fileops::{self, Transfer};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::folder_view::FolderView;
use rf_win::ctxmenu::ExtraItem;

const MENU_NEW_TAB: u32 = 1;
const MENU_PIN: u32 = 2;
const MENU_COPY_PATH: u32 = 3;
const MENU_TERMINAL: u32 = 4;
const MENU_NEW_FOLDER: u32 = 5;
const MENU_NEW_TEXT: u32 = 6;
const MENU_PASTE: u32 = 7;
const MENU_TAG_BASE: u32 = 100;
const MENU_EXTRACT_HERE: u32 = 10;
const MENU_EXTRACT_TO: u32 = 11;
const MENU_ZIP: u32 = 12;
const MENU_7Z: u32 = 13;
const MENU_TAGS: u32 = 14;
const MENU_OPEN_LOCATION: u32 = 15;
const MENU_BULK_RENAME: u32 = 16;
const MENU_SHELF: u32 = 17;
const MENU_OTHER_OPEN: u32 = 18;
const MENU_OTHER_COPY: u32 = 19;
const MENU_OTHER_MOVE: u32 = 20;

/// "photos.tar.gz" → "photos" (pure string work; the unique folder is chosen at extract time).
pub(crate) fn archive_stem(p: &std::path::Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_lowercase();
    for suf in [".tar.gz", ".tar.zst", ".tar.xz", ".tar.bz2", ".tgz", ".tzst", ".txz", ".tbz2", ".tbz", ".zip", ".7z", ".rar", ".iso", ".tar", ".gz", ".xz", ".zst", ".bz2"] {
        if lower.ends_with(suf) && name.len() > suf.len() && name.is_char_boundary(name.len() - suf.len()) {
            return name[..name.len() - suf.len()].to_string();
        }
    }
    name
}

/// What "Flatten folder" did, for undo / redo: every file's (old, new) path and the folders it
/// removed.
#[derive(Debug, Clone, Default)]
pub struct Flattened {
    pub moves: Vec<(PathBuf, PathBuf)>,
    pub removed: Vec<PathBuf>,
}

/// Worker of "Flatten folder". `rec` gets every step that happened, also when it fails or is
/// cancelled half way, so even that can be undone.
fn flatten(dir: &std::path::Path, t: &fileops::Tracker, rec: &mut Flattened) -> anyhow::Result<bool> {
    use std::sync::atomic::Ordering;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            let ft = e.file_type()?;
            if ft.is_dir() && !ft.is_symlink() {
                stack.push(p.clone());
                dirs.push(p);
            } else if d != dir {
                files.push(p);
            }
        }
    }
    let total = files.len() as u32;
    for (i, f) in files.iter().enumerate() {
        if t.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let name = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
        (t.on_progress)(total, i as u32, Some(name.clone()));
        let mut dest = dir.join(&name);
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
            _ => (name.clone(), String::new()),
        };
        let mut n = 2;
        while dest.exists() {
            dest = dir.join(format!("{stem} ({n}){ext}"));
            n += 1;
        }
        std::fs::rename(f, &dest)?;
        rec.moves.push((f.clone(), dest));
    }
    // deepest first: a folder is empty once its children are gone
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        if std::fs::remove_dir(&d).is_ok() {
            rec.removed.push(d);
        }
    }
    (t.on_progress)(total, total, None);
    Ok(true)
}

/// Undo of a flatten: the folders come back, every file goes back to where it was. A file
/// that is gone, or whose old place is taken by a new one, is left alone (and reported).
pub(crate) fn unflatten(rec: &Flattened) -> anyhow::Result<bool> {
    // shallowest first (create_dir_all would manage anyway; this keeps it obvious)
    let mut dirs = rec.removed.clone();
    dirs.sort_by_key(|d| d.components().count());
    for d in &dirs {
        std::fs::create_dir_all(d)?;
    }
    let mut failed = 0usize;
    for (old, new) in rec.moves.iter().rev() {
        if old.exists() || !new.exists() {
            failed += 1;
            continue;
        }
        if let Some(parent) = old.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::rename(new, old).is_err() {
            failed += 1;
        }
    }
    anyhow::ensure!(failed == 0, "{failed} {}", t!("dosya eski yerine taşınamadı"));
    Ok(true)
}

/// Redo of a flatten: the same moves again, then the same folders removed (if still empty).
pub(crate) fn reflatten(rec: &Flattened) -> anyhow::Result<bool> {
    let mut failed = 0usize;
    for (old, new) in &rec.moves {
        if new.exists() || !old.exists() || std::fs::rename(old, new).is_err() {
            failed += 1;
        }
    }
    let mut dirs = rec.removed.clone();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(&d);
    }
    anyhow::ensure!(failed == 0, "{failed} {}", t!("dosya taşınamadı"));
    Ok(true)
}

fn bg(name: &'static str, f: impl FnOnce() -> anyhow::Result<bool> + Send + 'static) {
    std::thread::Builder::new()
        .name(format!("rf-op-{name}"))
        .spawn(move || {
            if let Err(e) = f() {
                log::warn!("{name}: {e:#}");
            }
        })
        .ok();
}

impl FolderView {
    pub fn current_dir(&self) -> Option<PathBuf> {
        if let rf_core::Location::Library { file, .. } = self.location() {
            // new items / paste go to the library's default save folder
            return self.library_save_dir(file);
        }
        self.location().path().map(|p| p.to_path_buf())
    }

    /// Default save folder of a library (resolved in the background by `Places`).
    fn library_save_dir(&self, file: &std::path::Path) -> Option<PathBuf> {
        self.library_save.as_ref().filter(|(f, _)| f == file).map(|(_, d)| d.clone())
    }

    /// A shell folder (a phone…) is read-only here: says so and returns true.
    pub(crate) fn shell_read_only(&self, cx: &mut Context<Self>) -> bool {
        let shell = matches!(self.location(), rf_core::Location::Shell { .. });
        if shell {
            crate::shell_view::read_only_notice(cx);
        }
        shell
    }

    pub fn copy_to_clipboard(&mut self, kind: Transfer, _window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.location(), rf_core::Location::Shell { .. }) {
            // items of a phone: copied out with the shell's engine; cut becomes copy
            crate::shell_view::copy_items(cx, self.selection_shell());
            return;
        }
        let paths = self.selection_paths();
        if paths.is_empty() {
            return;
        }
        let in_archive = self.current_dir().is_some_and(|d| crate::archive_view::is_browsable_path(&d));
        if in_archive {
            // items inside an archive: extract them (Status Center), then the copies go on
            // the clipboard; cut becomes copy
            crate::archive_view::copy_items(cx, paths);
            return;
        }
        // OpenClipboard can wait on other apps: never on the UI thread.
        rf_win::sta::run(move |owner| {
            if let Err(e) = rf_win::clipboard::set_files(owner, &paths, kind) {
                log::warn!("clipboard: {e:#}");
            }
        });
    }

    /// Ctrl+Shift+C: plain paths, one per line (Ctrl+Alt+C: with quotes, see `copy_path_with`).
    pub fn copy_path(&mut self, cx: &mut Context<Self>) {
        self.copy_path_with(false, cx)
    }

    pub fn copy_path_with(&mut self, quoted: bool, cx: &mut Context<Self>) {
        let paths = self.selection_paths();
        let q = |s: String| if quoted { format!("\"{s}\"") } else { s };
        let text = if paths.is_empty() {
            self.current_dir().map(|d| q(d.to_string_lossy().into_owned())).unwrap_or_default()
        } else {
            paths.iter().map(|p| q(p.display().to_string())).collect::<Vec<_>>().join("\r\n")
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Files' "Rotate left / right" on the selected images (JPEG: orientation tag only, others
    /// rotated losslessly through WIC). The list reloads; new thumbnails follow the new mtime.
    pub fn rotate_selection(&mut self, clockwise: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .selection_paths()
            .into_iter()
            .filter(|p| rf_win::rotate::can_rotate(p) && crate::archive_view::split(p).is_none())
            .collect();
        if paths.is_empty() {
            return;
        }
        let task = cx.background_spawn(async move {
            let mut errors = Vec::new();
            for p in &paths {
                if let Err(e) = rf_win::rotate::rotate(p, clockwise) {
                    errors.push(format!("{}: {e:#}", p.file_name().unwrap_or_default().to_string_lossy()));
                }
            }
            errors
        });
        cx.spawn_in(window, async move |this, cx| {
            let errors = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                v.soft_reload(cx);
                if !errors.is_empty() {
                    log::warn!("rotate: {}", errors.join("; "));
                    crate::prompt::show(
                        t!("Döndürülemedi"),
                        Some(errors.join("
")),
                        None,
                        vec![crate::prompt::Button::new(t!("Tamam"), crate::prompt::Kind::Primary, |_, _, _| Ok(()))],
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_read_only(cx) {
            return;
        }
        let Some(dir) = self.current_dir() else { return };
        self.paste_to(dir, window, cx);
    }

    /// Ctrl+Shift+V: paste into the selected folder (falls back to the current one).
    pub fn paste_into_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_read_only(cx) {
            return;
        }
        let target = if self.selected_is_single_dir() { self.selection_paths().into_iter().next() } else { None };
        match target.or_else(|| self.current_dir()) {
            Some(dir) => self.paste_to(dir, window, cx),
            None => {}
        }
    }

    /// Creates shortcuts (.lnk) to the clipboard items in the current folder.
    pub fn paste_shortcuts(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_read_only(cx) {
            return;
        }
        let Some(dir) = self.current_dir() else { return };
        if rf_win::ftp::is_ftp(&dir) {
            return;
        }
        let (tx, rx) = oneshot::channel();
        rf_win::sta::run(move |owner| {
            let _ = tx.send(rf_win::clipboard::get_files(owner).map_err(|e| log::warn!("paste: {e:#}")).ok().flatten());
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((paths, _))) = rx.await else { return };
            let work = cx.background_spawn(async move {
                for p in paths {
                    let stem = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Kısayol".into());
                    // never overwrite: "x.lnk", "x (2).lnk", …
                    let mut link = dir.join(format!("{stem}.lnk"));
                    let mut n = 2;
                    while link.exists() {
                        link = dir.join(format!("{stem} ({n}).lnk"));
                        n += 1;
                    }
                    if let Err(e) = rf_win::shortcut::create(&link, &p) {
                        log::warn!("shortcut {link:?}: {e:#}");
                    }
                }
            });
            work.await;
            let _ = this.update(cx, |v, cx| v.refresh(cx));
        })
        .detach();
    }

    fn paste_to(&mut self, dir: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        // GetClipboardData can block on a slow source app (delayed rendering).
        let (tx, rx) = oneshot::channel();
        rf_win::sta::run(move |owner| {
            let _ = tx.send(rf_win::clipboard::get_files(owner).map_err(|e| log::warn!("paste: {e:#}")).ok().flatten());
        });
        if rf_win::ftp::is_ftp(&dir) {
            // FTP: upload the clipboard files (folders are skipped), then refresh
            let url = dir.to_string_lossy().into_owned();
            cx.spawn(async move |this, cx| {
                if let Ok(Some((paths, _))) = rx.await {
                    let work = cx.background_spawn(async move {
                        for p in paths.iter().filter(|p| p.is_file()) {
                            if let Err(e) = rf_win::ftp::upload(p, &url) {
                                log::warn!("ftp upload {p:?}: {e}");
                            }
                        }
                    });
                    work.await;
                    let _ = this.update(cx, |v, cx| v.refresh(cx));
                }
            })
            .detach();
            return;
        }
        cx.spawn(async move |this, cx| {
            if let Ok(Some((paths, kind))) = rx.await {
                let _ = cx.update(|cx| {
                    let here = dir.clone();
                    crate::status_center::transfer_then(cx, kind, paths, dir, move |arrived, cx| {
                        // select what arrived, if this view still shows that folder
                        let names: Vec<String> = arrived.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
                        let _ = this.update(cx, |v, cx| {
                            if v.current_dir().as_deref() == Some(here.as_path()) {
                                v.select_when_listed(names, cx);
                            }
                        });
                    });
                });
            }
        })
        .detach();
    }

    /// Deletes the selection, asking first per Settings → "Confirm before deleting".
    pub fn delete_selected(&mut self, permanent: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::settings::DeletePref;
        if self.shell_read_only(cx) {
            return;
        }
        let paths = self.selection_paths();
        if paths.is_empty() {
            return;
        }
        // inside an archive: remove the entries from it (always asks: there is no recycle bin)
        if let Some((archive, _)) = crate::archive_view::split(&paths[0]) {
            let inners: Vec<String> = paths
                .iter()
                .filter_map(|p| crate::archive_view::split(p).filter(|(a, _)| *a == archive))
                .map(|(_, i)| i.to_string_lossy().replace('\\', "/"))
                .collect();
            let what = if inners.len() == 1 { format!("\"{}\"", inners[0].rsplit('/').next().unwrap_or_default()) } else { crate::i18n::items(inners.len()) };
            crate::prompt::show(
                t!("Arşivden silinsin mi?"),
                Some(format!("{what} {}", t!("arşivden kalıcı olarak silinecek."))),
                None,
                vec![
                    crate::prompt::Button::new(t!("Sil"), crate::prompt::Kind::Danger, move |_, _, cx| {
                        crate::archive_view::delete_inside(cx, archive.clone(), inners.clone());
                        Ok(())
                    }),
                    crate::prompt::Button::cancel(),
                ],
                window,
                cx,
            );
            return;
        }
        let ask = match cx.global::<crate::settings::Settings>().confirm_delete {
            DeletePref::Always => true,
            DeletePref::PermanentOnly => permanent,
            DeletePref::Never => false,
        };
        if !ask {
            crate::status_center::delete(cx, paths, permanent);
            return;
        }
        let what = if paths.len() == 1 {
            format!("\"{}\"", rf_core::history::display_name(&paths[0]))
        } else {
            crate::i18n::items(paths.len())
        };
        let (title, msg) = if permanent {
            (t!("Kalıcı olarak silinsin mi?"), format!("{what} {}", t!("geri alınamayacak şekilde silinecek.")))
        } else {
            (t!("Geri dönüşüm kutusuna taşınsın mı?"), format!("{what} {}", t!("geri dönüşüm kutusuna taşınacak.")))
        };
        crate::prompt::show(
            title,
            Some(msg),
            None,
            vec![
                crate::prompt::Button::new(t!("Sil"), if permanent { crate::prompt::Kind::Danger } else { crate::prompt::Kind::Primary }, move |_, _, cx| {
                    crate::status_center::delete(cx, paths.clone(), permanent);
                    Ok(())
                }),
                crate::prompt::Button::cancel(),
            ],
            window,
            cx,
        );
    }

    pub fn create_new(&mut self, folder: bool, cx: &mut Context<Self>) {
        if self.shell_read_only(cx) {
            return;
        }
        let Some(dir) = self.current_dir() else { return };
        // inside an archive: a folder entry, or an empty text file added to it
        if let Some((archive, inner)) = crate::archive_view::split(&dir) {
            let taken: Vec<String> = self.entries.iter().map(|e| e.name.to_string()).collect();
            let inner = inner.to_string_lossy().replace('\\', "/").trim_matches('/').to_string();
            if folder {
                let name = crate::archive_view::unique_name(t!("Yeni klasör"), "", &taken);
                crate::archive_view::mkdir_inside(cx, archive, inner, name.clone());
                self.set_reselect(name);
            } else {
                let name = crate::archive_view::unique_name(t!("Yeni metin belgesi"), ".txt", &taken);
                let tmp = std::env::temp_dir().join(format!("smoothyfiles-new-{}", std::process::id()));
                let file = tmp.join(&name);
                if std::fs::create_dir_all(&tmp).and_then(|_| std::fs::write(&file, b"")).is_ok() {
                    crate::archive_view::add_into(cx, archive, std::path::PathBuf::from(inner), vec![file]);
                    self.set_reselect(name);
                }
            }
            return;
        }
        let task = cx.background_spawn(async move {
            if folder { fileops::new_folder(&dir) } else { fileops::new_text_file(&dir) }
        });
        cx.spawn(async move |this, cx| {
            if let Ok(path) = task.await {
                crate::undo::push(crate::undo::UndoOp::Created { path: path.clone() });
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let _ = this.update(cx, |v, cx| {
                    v.expect_new_item(name, cx);
                });
            }
        })
        .detach();
    }

    /// "Create folder with selection": a new folder here, the selected items moved into it,
    /// then rename the folder inline.
    pub fn folder_with_selection(&mut self, items: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir() else { return };
        if items.is_empty() {
            return;
        }
        let task = cx.background_spawn(async move { fileops::new_folder(&dir) });
        cx.spawn(async move |this, cx| {
            if let Ok(folder) = task.await {
                let name = folder.file_name().map(|n| n.to_string_lossy().into_owned());
                let _ = this.update(cx, |v, cx| {
                    crate::status_center::transfer(cx, Transfer::Move, items, folder.clone());
                    v.expect_new_item(name, cx);
                });
            }
        })
        .detach();
    }

    /// "Flatten folder": every file in the subfolders of `dir` moves up into `dir` (unique
    /// names on collisions), then the emptied subfolders are removed. Asks first.
    pub fn flatten_folder(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = rf_core::history::display_name(&dir);
        crate::prompt::show(
            t!("Klasör düzleştirilsin mi?"),
            Some(format!("{} \"{name}\" {}", t!("Alt klasörlerdeki tüm dosyalar"), t!("içine taşınır ve boş kalan alt klasörler silinir."))),
            None,
            vec![
                crate::prompt::Button::new(t!("Düzleştir"), crate::prompt::Kind::Primary, move |_, _, cx| {
                    let dir = dir.clone();
                    crate::status_center::start(cx, format!("{} {name}", t!("Düzleştiriliyor:")), move |t| {
                        let mut rec = Flattened::default();
                        let r = flatten(&dir, &t, &mut rec);
                        if !rec.moves.is_empty() || !rec.removed.is_empty() {
                            crate::undo::push(crate::undo::UndoOp::Flattened { dir: dir.clone(), rec });
                        }
                        r
                    });
                    Ok(())
                }),
                crate::prompt::Button::cancel(),
            ],
            window,
            cx,
        );
    }

    pub fn begin_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_read_only(cx) {
            return;
        }
        let Some(ei) = self.cursor_entry() else { return };
        let e = self.entry(ei);
        // Extensions hidden: edit only the name and keep the extension (as Explorer does)
        let name = match self.hidden_extension(ei, cx) {
            Some(_) => e.name[..e.stem_len()].to_string(),
            None => e.name.to_string(),
        };
        let sel_end = if e.is_dir() { name.len() } else { e.stem_len().min(name.len()) };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(window, cx),
            _ => {}
        });
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.set_selected_range(0..sel_end, cx);
        });
        self.set_renaming(Some((ei, input)), Some(sub));
        cx.notify();
    }

    pub fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_renaming(None, None);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The extension (with its dot) that the list hides for this entry, if any.
    fn hidden_extension(&self, ei: u32, cx: &App) -> Option<String> {
        let e = self.entry(ei);
        let hidden = !cx.global::<crate::settings::Settings>().show_extensions && !e.is_dir() && !e.ext.is_empty();
        hidden.then(|| e.name[e.stem_len()..].to_string())
    }

    pub fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ei, input)) = self.renaming.take() else { return };
        let typed = input.read(cx).value().trim().to_string();
        let new_name = match self.hidden_extension(ei, cx) {
            Some(ext) if !typed.is_empty() => format!("{typed}{ext}"),
            _ => typed,
        };
        let old = self.entry(ei).name.to_string();
        self.set_renaming(None, None);
        // Explorer's rule, said up front instead of a shell error after the fact
        const INVALID: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
        if new_name.contains(INVALID) || new_name.ends_with('.') {
            let title = crate::t!("Yeniden adlandır").to_string();
            let msg = format!("{}\n\n{}", new_name, crate::t!("Dosya adı şu karakterleri içeremez: \\ / : * ? \" < > | ve nokta ile bitemez."));
            rf_win::sta::run(move |owner| rf_win::shell::alert(owner, &title, &msg));
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        // inside an archive: rewrite the archive with the new name
        if !new_name.is_empty() && new_name != old {
            let from_path = self.parent_of(ei).map(|d| d.join(&old));
            if let Some((archive, inner)) = from_path.as_deref().and_then(crate::archive_view::split) {
                let inner = inner.to_string_lossy().replace('\\', "/");
                crate::archive_view::rename_inside(cx, archive, inner, new_name.clone());
                self.set_reselect(new_name);
                window.focus(&self.focus, cx);
                cx.notify();
                return;
            }
        }
        if !new_name.is_empty() && new_name != old {
            if let Some(dir) = self.parent_of(ei).map(|d| d.to_path_buf()) {
                let from = dir.join(&old);
                let to = dir.join(&new_name);
                self.set_reselect(new_name.clone());
                // Explorer-style warning when a file's extension changes.
                let ext = |n: &str| std::path::Path::new(n).extension().map(|e| e.to_string_lossy().to_lowercase());
                let warn = cx.global::<crate::settings::Settings>().warn_extension_change;
                let confirm_rx = (warn && !self.entry(ei).is_dir() && ext(&old).is_some() && ext(&old) != ext(&new_name)).then(|| {
                    let (t, r) = oneshot::channel::<bool>();
                    let title = crate::t!("Dosya adı uzantısı").to_string();
                    let msg = crate::t!("Dosya adı uzantısını değiştirirseniz dosya kullanılamaz hale gelebilir. Değiştirmek istediğinizden emin misiniz?").to_string();
                    rf_win::sta::run(move |owner| {
                        let _ = t.send(rf_win::shell::confirm(owner, &title, &msg));
                    });
                    r
                });
                let (f2, n2) = (from.clone(), new_name.clone());
                cx.spawn(async move |this, cx| {
                    if let Some(r) = confirm_rx {
                        if !r.await.unwrap_or(false) {
                            let _ = this.update(cx, |v, _| v.clear_reselect());
                            return;
                        }
                    }
                    // Another item already has that name (a case-only change is fine): say so
                    // plainly instead of the shell's "parameter is incorrect".
                    let (src, dst) = (f2.clone(), f2.with_file_name(&n2));
                    let clash = cx
                        .background_spawn(async move {
                            let case_only = src.to_string_lossy().to_lowercase() == dst.to_string_lossy().to_lowercase();
                            !case_only && std::fs::symlink_metadata(&dst).is_ok()
                        })
                        .await;
                    if clash {
                        let title = crate::t!("Yeniden adlandır").to_string();
                        let msg = format!("{}\n\n{}", n2, crate::t!("Bu adda bir öğe zaten var. Başka bir ad deneyin."));
                        rf_win::sta::run(move |owner| rf_win::shell::alert(owner, &title, &msg));
                        let _ = this.update(cx, |v, _| v.clear_reselect());
                        return;
                    }
                    let (tx, rx) = oneshot::channel::<anyhow::Result<bool>>();
                    bg("rename", move || {
                        let _ = tx.send(fileops::rename(&f2, &n2));
                        Ok(true)
                    });
                    match rx.await {
                        Ok(Ok(true)) => {
                            crate::undo::push(crate::undo::UndoOp::Renamed { from: from.clone(), to: to.clone() });
                            let _ = cx.update(|cx| crate::tags::update(cx, |t| t.moved(&from, &to)));
                        }
                        other => {
                            if let Ok(Err(e)) = other {
                                log::warn!("rename: {e:#}");
                            }
                            // nothing to wait for
                            let _ = this.update(cx, |v, _| v.clear_reselect());
                        }
                    }
                })
                .detach();
            }
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The classic Explorer context menu (Shift+right-click) for the selection or background.
    pub fn show_native_menu(&mut self, background: bool, cx: &mut Context<Self>) {
        let Some(dir) = self.current_dir() else { return };
        let items = if background { Vec::new() } else { self.selection_paths() };
        let background = background || items.is_empty();
        // From the listing, not the disk: a stat here could hang on an offline network path.
        let single_dir = items.len() == 1 && self.selected_is_single_dir();
        let item = |id: u32, label: &str| ExtraItem { id, label: label.to_string(), enabled: true };
        let extra: Vec<ExtraItem> = if background {
            vec![
                item(MENU_NEW_FOLDER, t!("Yeni klasör\tCtrl+Shift+N")),
                item(MENU_NEW_TEXT, t!("Yeni metin belgesi")),
                ExtraItem { id: MENU_PASTE, label: t!("Yapıştır\tCtrl+V").into(), enabled: rf_win::clipboard::has_files() },
                item(MENU_TERMINAL, t!("Terminalde aç")),
                item(MENU_COPY_PATH, t!("Yolu kopyala")),
            ]
        } else {
            let mut v = Vec::new();
            if self.is_search() && items.len() == 1 {
                v.push(item(MENU_OPEN_LOCATION, t!("Dosya konumunu aç")));
            }
            if single_dir {
                v.push(item(MENU_NEW_TAB, t!("Yeni sekmede aç")));
                v.push(item(MENU_PIN, t!("Kenar çubuğuna sabitle")));
                v.push(item(MENU_TERMINAL, t!("Terminalde aç")));
            }
            v.push(item(MENU_COPY_PATH, t!("Yolu kopyala\tCtrl+Shift+C")));
            if items.len() > 1 && !self.is_search() {
                v.push(item(MENU_BULK_RENAME, t!("Toplu yeniden adlandır…\tF2")));
            }
            if items.len() == 1 && rf_archive::is_supported(&items[0]) {
                v.push(item(MENU_EXTRACT_HERE, t!("Buraya çıkar")));
                v.push(item(MENU_EXTRACT_TO, &format!("{} {}\\", t!("Klasöre çıkar:"), archive_stem(&items[0]))));
            }
            v.push(item(MENU_ZIP, t!("ZIP olarak sıkıştır")));
            v.push(item(MENU_7Z, t!("7z olarak sıkıştır")));
            v.push(item(MENU_TAGS, t!("Etiketle…")));
            v.push(item(MENU_SHELF, t!("Rafa ekle\tCtrl+Shift+S")));
            if self.dual {
                if single_dir {
                    v.push(item(MENU_OTHER_OPEN, t!("Diğer panelde aç")));
                }
                v.push(item(MENU_OTHER_COPY, t!("Diğer panele kopyala")));
                v.push(item(MENU_OTHER_MOVE, t!("Diğer panele taşı")));
            }
            v
        };
        let target_dir = if single_dir { items[0].clone() } else { dir.clone() };
        let tag_items = items.clone();
        let paste_dir_ui = dir.clone();
        let (tx, rx) = oneshot::channel::<(MenuOutcome, Option<(Vec<PathBuf>, Transfer)>)>();
        // Read Shift here: the shell thread has its own input state.
        let extended = unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 };
        rf_win::sta::run(move |owner| {
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
            }
            let r = if background {
                rf_win::ctxmenu::show_for_background(owner, &dir, (pt.x, pt.y), extended, &extra)
            } else {
                rf_win::ctxmenu::show_for_items(owner, &items, (pt.x, pt.y), extended, &extra)
            };
            let r = r.unwrap_or_else(|e| {
                log::warn!("context menu: {e:#}");
                MenuOutcome::Dismissed
            });
            // Read the clipboard right here on the shell thread; the UI starts the transfer.
            let clip = if r == MenuOutcome::Custom(MENU_PASTE) {
                rf_win::clipboard::get_files(owner).ok().flatten()
            } else {
                None
            };
            let _ = tx.send((r, clip));
        });
        cx.spawn(async move |this, cx| {
            let (outcome, clip) = rx.await.unwrap_or((MenuOutcome::Dismissed, None));
            let _ = this.update(cx, |v, cx| match outcome {
                MenuOutcome::Custom(MENU_PASTE) => {
                    if let Some((paths, kind)) = clip {
                        crate::status_center::transfer(cx, kind, paths, paste_dir_ui.clone());
                    }
                }
                MenuOutcome::Rename => {
                    v.request_rename();
                    cx.notify();
                }
                MenuOutcome::Custom(MENU_NEW_FOLDER) => v.create_new(true, cx),
                MenuOutcome::Custom(MENU_NEW_TEXT) => v.create_new(false, cx),
                MenuOutcome::Custom(MENU_COPY_PATH) => v.copy_path(cx),
                MenuOutcome::Custom(MENU_TERMINAL) => {
                    let d = target_dir.clone();
                    std::thread::spawn(move || {
                        let _ = rf_win::shell::open_terminal(&d);
                    });
                }
                MenuOutcome::Custom(MENU_NEW_TAB) => {
                    cx.emit(crate::folder_view::FolderEvent::OpenInNewTab(rf_core::Location::Path(target_dir.clone())))
                }
                MenuOutcome::Custom(MENU_PIN) => v.pin(target_dir.clone(), cx),
                MenuOutcome::Custom(MENU_EXTRACT_HERE) => {
                    if let Some(a) = tag_items.first() {
                        let dest = a.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                        crate::status_center::extract(cx, a.clone(), Some(dest));
                    }
                }
                MenuOutcome::Custom(MENU_EXTRACT_TO) => {
                    if let Some(a) = tag_items.first() {
                        crate::status_center::extract(cx, a.clone(), None);
                    }
                }
                MenuOutcome::Custom(id @ (MENU_ZIP | MENU_7Z)) => {
                    let format = if id == MENU_ZIP { rf_archive::Format::Zip } else { rf_archive::Format::SevenZ };
                    if let Some(first) = tag_items.first() {
                        let dir = first.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                        let stem = if tag_items.len() == 1 {
                            first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Arşiv".into())
                        } else {
                            rf_core::history::display_name(&dir)
                        };
                        let dest = dir.join(format!("{stem}.{}", format.extension()));
                        crate::status_center::compress(cx, tag_items.clone(), dest, format);
                    }
                }
                MenuOutcome::Custom(MENU_TAGS) => v.tag_menu(tag_items.clone(), cx),
                MenuOutcome::Custom(MENU_SHELF) => cx.emit(crate::folder_view::FolderEvent::AddToShelf(tag_items.clone())),
                MenuOutcome::Custom(MENU_OTHER_OPEN) => {
                    cx.emit(crate::folder_view::FolderEvent::OpenInOtherPane(rf_core::Location::Path(target_dir.clone())))
                }
                MenuOutcome::Custom(id @ (MENU_OTHER_COPY | MENU_OTHER_MOVE)) => {
                    let kind = if id == MENU_OTHER_COPY { Transfer::Copy } else { Transfer::Move };
                    cx.emit(crate::folder_view::FolderEvent::ToOtherPane(kind, tag_items.clone()))
                }
                MenuOutcome::Custom(MENU_BULK_RENAME) => cx.emit(crate::folder_view::FolderEvent::BulkRename(tag_items.clone())),
                MenuOutcome::Custom(MENU_OPEN_LOCATION) => {
                    if let Some(p) = tag_items.first() {
                        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
                            v.set_reselect(name.to_string_lossy().into_owned());
                            v.navigate(rf_core::Location::Path(parent.to_path_buf()), cx);
                        }
                    }
                }
                _ => {}
            });
        })
        .detach();
    }

    /// Starts an OLE drag of the selection (to Explorer, the desktop, another tab…).
    pub fn start_drag_out(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        let items = self.selection_paths();
        if items.is_empty() {
            return;
        }
        let ui_thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        }
        rf_win::sta::run(move |owner| {
            if let Err(e) = rf_win::dnd::drag_out(owner, ui_thread, &items) {
                log::warn!("drag: {e:#}");
            }
        });
    }

    /// Files dropped onto `dest` (a folder row or the current folder background).
    pub fn drop_into(&mut self, dest: PathBuf, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let sources: Vec<PathBuf> = paths.paths().to_vec();
        if sources.is_empty() || rf_win::dnd::is_invalid_drop(&sources, &dest) {
            return;
        }
        let (ctrl, shift) = unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
            (GetKeyState(VK_CONTROL.0 as i32) < 0, GetKeyState(VK_SHIFT.0 as i32) < 0)
        };
        let kind = rf_win::dnd::default_transfer(&sources, &dest, ctrl, shift);
        crate::status_center::transfer(cx, kind, sources, dest);
    }

    /// Second popup listing tags (● = every selected item has it); picking toggles the tag.
    pub fn tag_menu(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let store = &cx.global::<crate::tags::Tags>().0;
        let items: Vec<ExtraItem> = store
            .defs
            .iter()
            .map(|d| {
                let all = paths.iter().all(|p| store.has(p, d.id));
                ExtraItem { id: MENU_TAG_BASE + d.id, label: format!("{}  {}", if all { "●" } else { "○" }, crate::i18n::td(&d.name)), enabled: true }
            })
            .collect();
        let (tx, rx) = oneshot::channel::<Option<u32>>();
        rf_win::sta::run(move |owner| {
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
            }
            let _ = tx.send(rf_win::ctxmenu::show_simple(owner, (pt.x, pt.y), &items));
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(id)) = rx.await {
                let _ = this.update(cx, |_, cx| {
                    crate::tags::update(cx, |t| t.toggle(&paths, id - MENU_TAG_BASE));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn pin(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let current: Vec<PathBuf> = self.places_pinned_paths(cx);
        crate::settings::update(cx, |s| {
            let mut v = s.pinned.clone().unwrap_or(current);
            if !v.contains(&dir) {
                v.push(dir);
            }
            s.pinned = Some(v);
        });
        self.refresh_places(cx);
    }

    /// Our Properties window for the selection (or the current folder when nothing is selected).
    pub fn properties(&self, cx: &mut App) {
        let mut paths = self.selection_paths();
        if paths.is_empty() {
            paths.extend(self.current_dir());
        }
        crate::properties_view::open(paths, self.images.clone(), cx);
    }

    pub fn open_with(&self) {
        if let Some(p) = self.selection_paths().into_iter().next() {
            rf_win::sta::run(move |_| {
                let _ = rf_win::shell::open_with(&p);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    // not `super::*`: gpui's `test` macro would shadow the built-in one
    use super::{Flattened, fileops, flatten, reflatten, unflatten};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn tree(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("Gün1/derin")).unwrap();
        std::fs::create_dir_all(root.join("Gün2")).unwrap();
        std::fs::create_dir_all(root.join("bos")).unwrap();
        std::fs::write(root.join("kok.txt"), b"kok").unwrap();
        std::fs::write(root.join("Gün1/a.mp4"), b"a1").unwrap();
        std::fs::write(root.join("Gün1/derin/a.mp4"), b"a2").unwrap();
        std::fs::write(root.join("Gün2/b.mp4"), b"b").unwrap();
    }

    fn listing(root: &std::path::Path) -> Vec<(String, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    stack.push(e.path());
                    out.push((rel, None));
                } else {
                    out.push((rel, Some(std::fs::read(e.path()).unwrap())));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn flatten_undo_redo() {
        let root = std::env::temp_dir().join(format!("rf-flatten-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        tree(&root);
        let before = listing(&root);
        let t = fileops::Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false)));

        let mut rec = Flattened::default();
        assert!(flatten(&root, &t, &mut rec).unwrap());
        let flat = listing(&root);
        // same-named files don't overwrite each other; every folder is gone
        assert_eq!(flat.iter().filter(|(_, c)| c.is_none()).count(), 0, "{flat:?}");
        assert_eq!(flat.len(), 4, "{flat:?}");
        assert!(root.join("a (2).mp4").exists());
        assert_eq!(rec.moves.len(), 3);
        assert_eq!(rec.removed.len(), 4); // Gün1, Gün1/derin, Gün2, bos

        assert!(unflatten(&rec).unwrap());
        assert_eq!(listing(&root), before); // folders (also the empty one) and files back

        assert!(reflatten(&rec).unwrap());
        assert_eq!(listing(&root), flat);

        // a file deleted after flattening: the rest still goes back, the miss is reported
        std::fs::remove_file(root.join("b.mp4")).unwrap();
        assert!(unflatten(&rec).is_err());
        assert!(root.join("Gün1/derin/a.mp4").exists() && root.join("Gün1/a.mp4").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
