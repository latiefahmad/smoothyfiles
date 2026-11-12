//! Our own Properties window, with the tabs of Windows' property sheet:
//! - General: name, type, the app it opens with (and "Change…"), location, size / size on disk /
//!   contents, times, attributes (read-only, hidden, and the advanced ones: archive, indexing,
//!   compression, encryption; for a folder optionally down its whole tree), "Unblock" for a
//!   downloaded file. A drive shows its file system and how full it is instead.
//! - Shortcut (.lnk): target, arguments, start in, comment, how it runs, run as administrator,
//!   its icon.
//! - Compatibility (programs and shortcuts to them): compatibility mode, reduced colour, 640×480,
//!   full-screen optimisations, run as administrator, restart registration, ICC, high-DPI
//!   override — per user, as Windows stores them.
//! - Security: who may do what (read from the file's ACL), and the owner.
//! - Details: the version resource of programs, media / document properties, the file facts.
//! - Digital signatures: the embedded Authenticode signatures and whether Windows trusts them.
//! - Sharing (folders): the network name it is shared as.
//! - Customize (folders): the folder's own icon.
//! - Hashes (files).
//!
//! Every disk access happens on background threads; the window never waits on a slow drive.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::{Entry, EntryKind, format};
use rf_win::compat::Compat;
use rf_win::fileinfo::{ARCHIVE, COMPRESSED, ENCRYPTED, Info, NOT_INDEXED};

use crate::image_cache::ImageCache;
use crate::select_box::{Choice, select_box};
use crate::t;
use crate::theme::{self, Palette};

const READONLY: u32 = 0x1;
const HIDDEN: u32 = 0x2;

/// Opens a Properties window for `paths` (one window per call), on the display of the window
/// in front.
pub fn open(paths: Vec<PathBuf>, images: Entity<ImageCache>, cx: &mut App) {
    if paths.is_empty() {
        return;
    }
    let display = cx.active_window().and_then(|w| w.update(cx, |_, w, cx| w.display(cx).map(|d| d.id())).ok().flatten());
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(display, size(px(500.), px(680.)), cx))),
        display_id: display,
        window_min_size: Some(size(px(420.), px(460.))),
        window_background: WindowBackgroundAppearance::MicaBackdrop,
        ..TitleBar::window_options()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, move |window, cx| cx.new(|cx| PropsView::new(paths, images, window, cx))) {
        log::error!("properties window: {e:#}");
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    General,
    Shortcut,
    Compat,
    Security,
    Details,
    Signatures,
    Sharing,
    Customize,
    Hashes,
}

/// Totals for a multi-selection.
#[derive(Default, Clone)]
struct Totals {
    files: u64,
    dirs: u64,
    bytes: u64,
    on_disk: u64,
    /// Selected folders (the selection itself, not what is inside).
    sel_dirs: u64,
    /// None before the first file; Some(Some(ext)) while every selected file has `ext`
    /// ("" = none); Some(None) once they differ.
    same_ext: Option<Option<String>>,
    done: bool,
}

/// A checkbox over one attribute bit: None = mixed across the selection; `was` = on load.
#[derive(Clone, Copy, Default, PartialEq)]
struct Flag {
    now: Option<bool>,
    was: Option<bool>,
}

impl Flag {
    fn load(v: Option<bool>) -> Self {
        Flag { now: v, was: v }
    }
    fn changed(&self) -> bool {
        self.now != self.was
    }
    fn toggle(&mut self) {
        self.now = Some(!self.now.unwrap_or(false));
    }
    fn commit(&mut self) {
        self.was = self.now;
    }
}

/// The attribute checkboxes (indexed = NOT_CONTENT_INDEXED clear).
#[derive(Clone, Copy, Default, PartialEq)]
struct Attrs {
    readonly: Flag,
    hidden: Flag,
    archive: Flag,
    indexed: Flag,
    compressed: Flag,
    encrypted: Flag,
}

impl Attrs {
    fn all(&mut self) -> [&mut Flag; 6] {
        [&mut self.readonly, &mut self.hidden, &mut self.archive, &mut self.indexed, &mut self.compressed, &mut self.encrypted]
    }
    fn changed(&self) -> bool {
        let mut c = *self;
        c.all().iter().any(|f| f.changed())
    }
    fn commit(&mut self) {
        for f in self.all() {
            f.commit();
        }
    }
}

/// Folds one item's attribute bits into the selection's tri-states.
fn merge_attrs(acc: &mut [Option<bool>; 6], attrs: u32, first: bool) {
    let bits = [
        attrs & READONLY != 0,
        attrs & HIDDEN != 0,
        attrs & ARCHIVE != 0,
        attrs & NOT_INDEXED == 0,
        attrs & COMPRESSED != 0,
        attrs & ENCRYPTED != 0,
    ];
    for (a, b) in acc.iter_mut().zip(bits) {
        *a = if first || *a == Some(b) { Some(b) } else { None };
    }
}

fn attrs_from(v: [Option<bool>; 6]) -> Attrs {
    Attrs {
        readonly: Flag::load(v[0]),
        hidden: Flag::load(v[1]),
        archive: Flag::load(v[2]),
        indexed: Flag::load(v[3]),
        compressed: Flag::load(v[4]),
        encrypted: Flag::load(v[5]),
    }
}

/// What a single item's background load brings.
#[derive(Default)]
struct Loaded {
    info: Option<Result<Info, String>>,
    blocked: bool,
    /// The volume can compress / encrypt.
    caps: (bool, bool),
    drive: Option<rf_win::drives::Drive>,
    share: Option<String>,
    pe: Option<rf_win::pe::PeInfo>,
    signatures: Option<rf_win::signature::Signatures>,
    link: Option<rf_win::shortcut::Details>,
    /// The program the Compatibility tab is about (the file, or the shortcut's target).
    compat_exe: Option<PathBuf>,
    compat: Compat,
    compat_all_users: bool,
    folder_icon: Option<(String, i32)>,
}

/// The shortcut tab's text fields.
struct LinkInputs {
    target: Entity<InputState>,
    args: Entity<InputState>,
    dir: Entity<InputState>,
    comment: Entity<InputState>,
}

pub struct PropsView {
    paths: Vec<PathBuf>,
    images: Entity<ImageCache>,
    page: Page,
    info: Option<Result<Info, String>>,
    folder: Option<Option<rf_win::foldersize::FolderSize>>,
    totals: Option<Totals>,
    media: Vec<(&'static str, String)>,
    name: Option<Entity<InputState>>,
    attrs: Attrs,
    /// The volume can compress / encrypt (Advanced attributes shows those only then).
    caps: (bool, bool),
    advanced_open: bool,
    /// A folder's attribute change goes down its whole tree too.
    recurse: bool,
    /// A single downloaded file (Windows may block it), and whether the user ticked "Unblock".
    blocked: bool,
    unblock: bool,
    drive: Option<rf_win::drives::Drive>,
    share: Option<String>,
    pe: Option<rf_win::pe::PeInfo>,
    signatures: Option<rf_win::signature::Signatures>,
    sig_sel: usize,
    security: Option<Result<rf_win::security::Security, String>>,
    sec_sel: usize,
    link: Option<rf_win::shortcut::Details>,
    link_loaded: Option<rf_win::shortcut::Details>,
    link_inputs: Option<LinkInputs>,
    compat_exe: Option<PathBuf>,
    compat: Compat,
    compat_loaded: Compat,
    compat_all_users: bool,
    folder_icon: Option<(String, i32)>,
    folder_icon_loaded: Option<(String, i32)>,
    hash: Option<Entity<crate::hash_panel::HashPanel>>,
    error: Option<SharedString>,
    busy: bool,
    frame_dark: Option<bool>,
    cancel: Arc<AtomicBool>,
    focus: FocusHandle,
}

impl Drop for PropsView {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Programs and other files that carry a version resource.
fn is_pe(p: &Path) -> bool {
    matches!(ext_of(p).as_str(), "exe" | "dll" | "sys" | "ocx" | "cpl" | "scr" | "drv" | "efi" | "mui" | "ax" | "com")
}

/// Files that can carry an embedded Authenticode signature.
fn signable(p: &Path) -> bool {
    is_pe(p) || matches!(ext_of(p).as_str(), "msi" | "msp" | "cab" | "cat" | "ps1" | "psm1" | "psd1" | "vbs" | "js" | "appx" | "msix" | "appxbundle" | "msixbundle")
}

/// A drive's root ("C:\").
fn is_root(p: &Path) -> bool {
    p.parent().is_none() && p.to_string_lossy().contains(':')
}

/// Text with a breaking point after each backslash (long paths wrap at folders).
fn breakable(s: &str) -> String {
    s.replace('\\', "\\\u{200B}")
}

impl PropsView {
    fn new(paths: Vec<PathBuf>, images: Entity<ImageCache>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&images, |_, _, cx| cx.notify()).detach();
        let single = paths.len() == 1 && !is_root(&paths[0]);
        let name = single.then(|| {
            let n = file_name(&paths[0]);
            cx.new(|cx| {
                let mut s = InputState::new(window, cx);
                s.set_value(n, window, cx);
                s
            })
        });
        let mut v = Self {
            paths,
            images,
            page: Page::General,
            info: None,
            folder: None,
            totals: None,
            media: Vec::new(),
            name,
            attrs: Attrs::default(),
            caps: (false, false),
            advanced_open: false,
            recurse: false,
            blocked: false,
            unblock: false,
            drive: None,
            share: None,
            pe: None,
            signatures: None,
            sig_sel: 0,
            security: None,
            sec_sel: 0,
            link: None,
            link_loaded: None,
            link_inputs: None,
            compat_exe: None,
            compat: Compat::default(),
            compat_loaded: Compat::default(),
            compat_all_users: false,
            folder_icon: None,
            folder_icon_loaded: None,
            hash: None,
            error: None,
            busy: false,
            frame_dark: None,
            cancel: Default::default(),
            focus: cx.focus_handle(),
        };
        v.load(window, cx);
        window.focus(&v.focus, cx);
        window.activate_window();
        v
    }

    fn single(&self) -> Option<&Path> {
        (self.paths.len() == 1).then(|| self.paths[0].as_path())
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cancel = self.cancel.clone();
        if let Some(path) = self.single().map(Path::to_path_buf) {
            let p = path.clone();
            let task = cx.background_spawn(async move { load_one(&p) });
            cx.spawn_in(window, async move |this, cx| {
                let l = task.await;
                let _ = this.update_in(cx, |v, window, cx| {
                    v.blocked = l.blocked;
                    v.caps = l.caps;
                    v.drive = l.drive;
                    v.share = l.share;
                    v.pe = l.pe;
                    v.signatures = l.signatures;
                    v.compat_exe = l.compat_exe;
                    v.compat = l.compat.clone();
                    v.compat_loaded = l.compat;
                    v.compat_all_users = l.compat_all_users;
                    v.folder_icon = l.folder_icon.clone();
                    v.folder_icon_loaded = l.folder_icon;
                    if let Some(d) = l.link {
                        let mk = |s: &str, window: &mut Window, cx: &mut Context<Self>| {
                            let s = s.to_string();
                            cx.new(|cx| {
                                let mut i = InputState::new(window, cx);
                                i.set_value(s, window, cx);
                                i
                            })
                        };
                        v.link_inputs = Some(LinkInputs {
                            target: mk(&d.target, window, cx),
                            args: mk(&d.args, window, cx),
                            dir: mk(&d.working_dir, window, cx),
                            comment: mk(&d.comment, window, cx),
                        });
                        v.link_loaded = Some(d.clone());
                        v.link = Some(d);
                    }
                    if let Some(Ok(i)) = &l.info {
                        let mut acc = [None; 6];
                        merge_attrs(&mut acc, i.attrs, true);
                        v.attrs = attrs_from(acc);
                        if v.drive.is_none() {
                            v.start_details(i.is_dir, cx);
                        }
                    }
                    v.info = l.info;
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.totals = Some(Totals::default());
            let paths = self.paths.clone();
            if let Some(first) = paths.first().cloned() {
                let task = cx.background_spawn(async move { rf_win::fileinfo::volume_caps(&first) });
                cx.spawn(async move |this, cx| {
                    let caps = task.await;
                    let _ = this.update(cx, |v, cx| {
                        v.caps = caps;
                        cx.notify();
                    });
                })
                .detach();
            }
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<(Totals, [Option<bool>; 6])>();
            std::thread::Builder::new()
                .name("rf-props".into())
                .spawn(move || {
                    let cache = crate::preview::size_cache();
                    let mut t = Totals::default();
                    let mut acc = [None; 6];
                    let mut first_file = true;
                    for (i, p) in paths.iter().enumerate() {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        let Ok(info) = rf_win::fileinfo::info(p) else { continue };
                        merge_attrs(&mut acc, info.attrs, i == 0);
                        if info.is_dir {
                            t.sel_dirs += 1;
                            t.dirs += 1;
                            if let Ok(fs) = cache.get_or_compute(p, &|| cancel.load(Ordering::Relaxed), &mut |_| {}) {
                                t.bytes += fs.bytes;
                                t.on_disk += fs.allocated.unwrap_or(fs.bytes);
                                t.files += fs.files;
                                t.dirs += fs.dirs;
                            }
                        } else {
                            t.files += 1;
                            t.bytes += info.size;
                            t.on_disk += info.size_on_disk.unwrap_or(info.size);
                            let e = ext_of(p);
                            t.same_ext = match &t.same_ext {
                                _ if first_file => Some(Some(e)),
                                Some(Some(x)) if *x == e => Some(Some(e)),
                                _ => Some(None),
                            };
                            first_file = false;
                        }
                        let _ = tx.unbounded_send((t.clone(), acc));
                    }
                    t.done = true;
                    let _ = tx.unbounded_send((t, acc));
                })
                .ok();
            cx.spawn(async move |this, cx| {
                use futures::StreamExt;
                let mut first = true;
                while let Some((t, acc)) = rx.next().await {
                    let done = t.done;
                    let ok = this
                        .update(cx, |v, cx| {
                            v.totals = Some(t);
                            if first || done {
                                v.attrs = attrs_from(acc);
                            }
                            cx.notify();
                        })
                        .is_ok();
                    first = false;
                    if !ok {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// Folder contents (folders) or media properties (files), after the basic info arrived.
    fn start_details(&mut self, is_dir: bool, cx: &mut Context<Self>) {
        let Some(path) = self.single().map(Path::to_path_buf) else { return };
        let cancel = self.cancel.clone();
        if is_dir {
            self.folder = Some(None);
            let task = cx.background_spawn(async move {
                crate::preview::size_cache().get_or_compute(&path, &|| cancel.load(Ordering::Relaxed), &mut |_| {}).ok()
            });
            cx.spawn(async move |this, cx| {
                let r = task.await;
                let _ = this.update(cx, |v, cx| {
                    v.folder = Some(r);
                    cx.notify();
                });
            })
            .detach();
        } else {
            let task = cx.background_spawn(async move { rf_win::props::media_props(&path) });
            cx.spawn(async move |this, cx| {
                let r = task.await;
                let _ = this.update(cx, |v, cx| {
                    v.media = r;
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// The Security tab reads the ACL when first shown.
    fn load_security(&mut self, cx: &mut Context<Self>) {
        if self.security.is_some() {
            return;
        }
        let p = self.paths[0].clone();
        let task = cx.background_spawn(async move { rf_win::security::read(&p).map_err(|e| format!("{e:#}")) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                v.security = Some(r);
                cx.notify();
            });
        })
        .detach();
        self.security = None;
    }

    fn link_now(&self, cx: &App) -> Option<rf_win::shortcut::Details> {
        let mut d = self.link.clone()?;
        if let Some(i) = &self.link_inputs {
            d.target = i.target.read(cx).value().trim().to_string();
            d.args = i.args.read(cx).value().to_string();
            d.working_dir = i.dir.read(cx).value().trim().to_string();
            d.comment = i.comment.read(cx).value().to_string();
        }
        Some(d)
    }

    fn renamed_to(&self, cx: &App) -> Option<String> {
        match (&self.name, self.single()) {
            (Some(n), Some(p)) => {
                let v = n.read(cx).value().trim().to_string();
                (v != file_name(p)).then_some(v)
            }
            _ => None,
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        self.renamed_to(cx).is_some()
            || self.attrs.changed()
            || (self.blocked && self.unblock)
            || self.link_now(cx) != self.link_loaded
            || self.compat != self.compat_loaded
            || self.folder_icon != self.folder_icon_loaded
    }

    /// Applies the edits on a helper thread; `close` closes the window when it worked.
    fn apply(&mut self, close: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !self.dirty(cx) {
            if close {
                window.remove_window();
            }
            return;
        }
        let new_name = self.renamed_to(cx);
        if let Some(n) = &new_name {
            if let Err(e) = rf_core::rename::validate(n) {
                self.error = Some(crate::i18n::td(&e.to_string()).into());
                cx.notify();
                return;
            }
        }
        // attribute bits to set / clear, compression and encryption to switch
        let a = self.attrs;
        let (mut set, mut clear) = (0u32, 0u32);
        for (flag, bit, inverted) in [(a.readonly, READONLY, false), (a.hidden, HIDDEN, false), (a.archive, ARCHIVE, false), (a.indexed, NOT_INDEXED, true)] {
            if let (true, Some(on)) = (flag.changed(), flag.now) {
                if on != inverted {
                    set |= bit;
                } else {
                    clear |= bit;
                }
            }
        }
        let compress = a.compressed.changed().then_some(a.compressed.now).flatten();
        let encrypt = a.encrypted.changed().then_some(a.encrypted.now).flatten();
        let recurse = self.recurse;
        let paths = self.paths.clone();
        let unblock = self.blocked && self.unblock;
        let link = self.link_now(cx).filter(|d| Some(d) != self.link_loaded.as_ref());
        let compat = (self.compat != self.compat_loaded).then(|| (self.compat_exe.clone(), self.compat.clone()));
        let icon = (self.folder_icon != self.folder_icon_loaded).then(|| self.folder_icon.clone());
        let cancel = self.cancel.clone();
        self.busy = true;
        self.error = None;
        let task = cx.background_spawn(async move {
            let mut paths = paths;
            let named = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", file_name(p));
            if unblock {
                rf_win::fileinfo::unblock(&paths[0]).map_err(|e| named(&paths[0], &e))?;
            }
            for p in &paths {
                let tree = recurse && p.is_dir();
                if set != 0 || clear != 0 {
                    if tree {
                        rf_win::fileinfo::set_attrs_tree(p, set, clear, &|| cancel.load(Ordering::Relaxed)).map_err(|e| named(p, &e))?;
                    } else {
                        rf_win::fileinfo::set_attrs(p, set, clear).map_err(|e| named(p, &e))?;
                    }
                }
                // (encryption and compression exclude each other: the one switched off first)
                let switches: Vec<(bool, bool)> = match (compress, encrypt) {
                    (Some(c), Some(e)) if c => vec![(false, e), (true, c)],
                    (Some(c), Some(e)) => vec![(true, c), (false, e)],
                    (Some(c), None) => vec![(true, c)],
                    (None, Some(e)) => vec![(false, e)],
                    (None, None) => vec![],
                };
                for (is_compress, on) in switches {
                    let items = if tree { walk(p, &cancel) } else { vec![p.clone()] };
                    for it in &items {
                        let r = if is_compress { rf_win::fileinfo::set_compressed(it, on) } else { rf_win::fileinfo::set_encrypted(it, on) };
                        r.map_err(|e| named(it, &e))?;
                    }
                }
            }
            if let Some(d) = &link {
                rf_win::shortcut::save(&paths[0], d).map_err(|e| named(&paths[0], &format!("{e:#}")))?;
            }
            if let Some((Some(exe), c)) = &compat {
                rf_win::compat::write(exe, c).map_err(|e| named(exe, &format!("{e:#}")))?;
            }
            if let Some(icon) = &icon {
                rf_win::folder_custom::set_folder_icon(&paths[0], icon.as_ref().map(|(f, i)| (f.as_str(), *i))).map_err(|e| named(&paths[0], &format!("{e:#}")))?;
            }
            let mut renamed = None;
            if let Some(n) = new_name {
                let from = paths[0].clone();
                let to = from.with_file_name(&n);
                if to.exists() && !n.eq_ignore_ascii_case(&file_name(&from)) {
                    return Err(t!("Bu adla bir öğe zaten var").to_string());
                }
                std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
                renamed = Some((from, to.clone()));
                paths[0] = to;
            }
            Ok::<_, String>((paths, renamed))
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                v.busy = false;
                match r {
                    Ok((paths, renamed)) => {
                        if let Some((from, to)) = renamed {
                            crate::undo::push(crate::undo::UndoOp::Renamed { from, to });
                        }
                        v.paths = paths;
                        v.attrs.commit();
                        v.recurse = false;
                        if v.unblock {
                            (v.blocked, v.unblock) = (false, false);
                        }
                        v.link_loaded = v.link_now(cx);
                        v.link = v.link_loaded.clone();
                        v.compat_loaded = v.compat.clone();
                        v.folder_icon_loaded = v.folder_icon.clone();
                        if close {
                            window.remove_window();
                        }
                    }
                    Err(e) => v.error = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn windows_properties(&self) {
        let p = self.paths[0].clone();
        rf_win::sta::run(move |_| {
            let _ = rf_win::shell::properties(&p);
        });
    }

    /// The HWND of this window, for dialogs it owns.
    fn hwnd(window: &Window) -> isize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        }
    }

    /// Windows' "Open with" chooser for the type; the new default shows when it closes.
    fn change_app(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.single().map(Path::to_path_buf) else { return };
        let owner = Self::hwnd(window);
        let (tx, rx) = futures::channel::oneshot::channel();
        rf_win::sta::run(move |_| {
            let _ = rf_win::shell::open_with_dialog(windows::Win32::Foundation::HWND(owner as *mut _), &p);
            let _ = tx.send(rf_win::fileinfo::opens_with(&p));
        });
        cx.spawn(async move |this, cx| {
            if let Ok(app) = rx.await {
                let _ = this.update(cx, |v, cx| {
                    if let Some(Ok(i)) = &mut v.info {
                        i.opens_with = app;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Windows' "Change Icon" dialog; `apply` takes the choice to the shortcut or the folder.
    fn pick_icon(&mut self, start: (String, i32), for_link: bool, window: &mut Window, cx: &mut Context<Self>) {
        let owner = Self::hwnd(window);
        let (tx, rx) = futures::channel::oneshot::channel();
        rf_win::sta::run(move |_| {
            let _ = tx.send(rf_win::folder_custom::pick_icon(windows::Win32::Foundation::HWND(owner as *mut _), &start.0, start.1));
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Some((file, index))) = rx.await {
                let _ = this.update(cx, |v, cx| {
                    if for_link {
                        if let Some(d) = &mut v.link {
                            d.icon = file;
                            d.icon_index = index;
                        }
                    } else {
                        v.folder_icon = Some((file, index));
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn entry(&self) -> Option<Entry> {
        let p = self.single()?;
        let Some(Ok(i)) = &self.info else { return None };
        let kind = if i.is_dir { EntryKind::Dir } else { EntryKind::File };
        Some(Entry::new(&file_name(p), kind, i.size, i.modified_ms, i.created_ms, i.attrs))
    }

    fn is_dir(&self) -> bool {
        matches!(&self.info, Some(Ok(i)) if i.is_dir)
    }

    fn pages(&self) -> Vec<(Page, &'static str)> {
        let mut pages = vec![(Page::General, t!("Genel"))];
        let Some(path) = self.single() else { return pages };
        let Some(Ok(info)) = &self.info else { return pages };
        if self.link.is_some() {
            pages.push((Page::Shortcut, t!("Kısayol")));
        }
        if self.compat_exe.is_some() {
            pages.push((Page::Compat, t!("Uyumluluk")));
        }
        pages.push((Page::Security, t!("Güvenlik")));
        if !info.is_dir {
            pages.push((Page::Details, t!("Ayrıntılar")));
        }
        if self.signatures.is_some() {
            pages.push((Page::Signatures, t!("Dijital imzalar")));
        }
        if info.is_dir {
            pages.push((Page::Sharing, t!("Paylaşım")));
            if self.drive.is_none() && !is_root(path) {
                pages.push((Page::Customize, t!("Özelleştir")));
            }
        } else {
            pages.push((Page::Hashes, t!("Karmalar")));
        }
        pages
    }

    // ---------- rendering ----------

    fn render_general(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(d) = &self.drive {
            return self.render_drive(d, p, cx);
        }
        let mut out = Vec::new();
        let mut card1 = Vec::new();
        if let Some(path) = self.single() {
            match &self.info {
                None => card1.push(row(p, t!("Durum"), t!("Yükleniyor…").into())),
                Some(Err(e)) => card1.push(row(p, t!("Hata"), e.clone())),
                Some(Ok(i)) => {
                    if let Some(e) = self.entry() {
                        let ext = ext_of(path);
                        let label = crate::icons::type_label(&e).to_string();
                        let label = if i.is_dir || ext.is_empty() { label } else { format!("{label} (.{ext})") };
                        card1.push(row(p, t!("Tür"), label));
                    }
                    // a program or shortcut runs itself: Windows shows its description there
                    let runs = matches!(ext_of(path).as_str(), "exe" | "com" | "scr" | "lnk" | "bat" | "cmd" | "msi" | "url");
                    if let Some(d) = self.pe.as_ref().and_then(|pe| pe.description.clone()).filter(|_| runs) {
                        card1.push(row(p, t!("Açıklama"), d));
                    }
                    if !i.is_dir && !runs {
                        let app = i.opens_with.clone().unwrap_or_else(|| t!("Seçilmedi").to_string());
                        card1.push(row_with(
                            p,
                            t!("Birlikte açılır"),
                            app,
                            link_button(p, "change-app", L::AppWindow, t!("Değiştir…")).on_click(cx.listener(|v, _, window, cx| v.change_app(window, cx))).into_any_element(),
                        ));
                    }
                    let loc = path.parent().map(|d| breakable(&d.to_string_lossy())).unwrap_or_default();
                    card1.push(row(p, t!("Konum"), loc));
                    if i.is_dir {
                        let (size, contents) = match &self.folder {
                            Some(Some(fs)) => (
                                format!("{} ({} {})", format::size(fs.bytes), group(fs.bytes), t!("bayt")),
                                format!("{} {}, {} {}", group(fs.files), t!("dosya"), group(fs.dirs), t!("klasör")),
                            ),
                            Some(None) if self.folder.is_some() => (t!("Hesaplanıyor…").into(), t!("Hesaplanıyor…").into()),
                            _ => ("—".into(), "—".into()),
                        };
                        card1.push(row(p, t!("Boyut"), size));
                        if let Some(d) = self.folder.as_ref().and_then(|f| f.as_ref()).and_then(|fs| fs.allocated) {
                            card1.push(row(p, t!("Disk üzerindeki boyut"), format!("{} ({} {})", format::size(d), group(d), t!("bayt"))));
                        }
                        card1.push(row(p, t!("İçerik"), contents));
                    } else {
                        card1.push(row(p, t!("Boyut"), format!("{} ({} {})", format::size(i.size), group(i.size), t!("bayt"))));
                        if let Some(d) = i.size_on_disk {
                            card1.push(row(p, t!("Disk üzerindeki boyut"), format!("{} ({} {})", format::size(d), group(d), t!("bayt"))));
                        }
                    }
                }
            }
        } else if let Some(t) = &self.totals {
            let dir = self.paths[0].parent().map(|d| breakable(&d.to_string_lossy())).unwrap_or_default();
            let sel_files = (self.paths.len() as u64).saturating_sub(t.sel_dirs);
            card1.push(row(p, t!("Seçim"), format!("{} {}, {} {}", group(sel_files), t!("dosya"), group(t.sel_dirs), t!("klasör"))));
            let kind = match (&t.same_ext, t.sel_dirs) {
                (Some(Some(e)), 0) if !e.is_empty() => format!("{} .{e}", t!("Tümü şu türde:")),
                (_, d) if d as usize == self.paths.len() => t!("Tümü klasör").to_string(),
                _ => t!("Birden çok tür").to_string(),
            };
            card1.push(row(p, t!("Tür"), kind));
            card1.push(row(p, t!("Konum"), format!("{} {dir}", t!("Tümü şurada:"))));
            let suffix = if t.done { String::new() } else { format!(" — {}", t!("hesaplanıyor…")) };
            card1.push(row(p, t!("Boyut"), format!("{} ({} {}){suffix}", format::size(t.bytes), group(t.bytes), t!("bayt"))));
            card1.push(row(p, t!("Disk üzerindeki boyut"), format!("{} ({} {}){suffix}", format::size(t.on_disk), group(t.on_disk), t!("bayt"))));
            card1.push(row(p, t!("İçerik"), format!("{} {}, {} {}", group(t.files), t!("dosya"), group(t.dirs), t!("klasör"))));
        }
        out.push(card(p, card1));

        if let Some(Ok(i)) = &self.info {
            out.push(card(
                p,
                vec![
                    row(p, t!("Oluşturulma"), format::datetime_full(i.created_ms)),
                    row(p, t!("Değiştirilme"), format::datetime_full(i.modified_ms)),
                    row(p, t!("Erişim"), format::datetime_full(i.accessed_ms)),
                ],
            ));
        }

        out.push(self.render_attrs(p, cx));

        if self.blocked {
            let on = self.unblock;
            out.push(card(
                p,
                vec![
                    section(p, t!("Güvenlik")),
                    note(p, t!("Bu dosya başka bir bilgisayardan geldi. Windows, bilgisayarınızı korumak için onu engelleyebilir.")),
                    check_row(p, "unblock", t!("Engellemeyi kaldır"), Some(on), cx.listener(move |v, _, _, cx| {
                        v.unblock = !on;
                        cx.notify();
                    })),
                ],
            ));
        }
        out.push(
            link_button(p, "win-props", L::ExternalLink, t!("Windows özellikleri (önceki sürümler ve diğerleri)…"))
                .on_click(cx.listener(|v, _, _, _| v.windows_properties()))
                .into_any_element(),
        );
        out
    }

    fn render_attrs(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let a = self.attrs;
        let dir = self.is_dir() || self.paths.iter().any(|p| p.is_dir());
        let flag = |id: &'static str, label: &'static str, f: Flag, pick: fn(&mut Attrs) -> &mut Flag, cx: &mut Context<Self>| {
            check_row(p, id, label, f.now, cx.listener(move |v, _, _, cx| {
                pick(&mut v.attrs).toggle();
                // compression and encryption exclude each other, as in Windows
                if id == "comp" && v.attrs.compressed.now == Some(true) {
                    v.attrs.encrypted.now = Some(false);
                }
                if id == "enc" && v.attrs.encrypted.now == Some(true) {
                    v.attrs.compressed.now = Some(false);
                }
                cx.notify();
            }))
        };
        let mut rows = vec![
            section(p, t!("Öznitelikler")),
            flag("ro", if dir { t!("Salt okunur (yalnızca klasördeki dosyalar için geçerlidir)") } else { t!("Salt okunur") }, a.readonly, |a| &mut a.readonly, cx),
            flag("hid", t!("Gizli"), a.hidden, |a| &mut a.hidden, cx),
        ];
        let open = self.advanced_open;
        rows.push(
            h_flex()
                .id("adv")
                .gap_1p5()
                .py(px(6.))
                .items_center()
                .text_sm()
                .text_color(p.accent)
                .cursor_pointer()
                .hover(|s| s.opacity(0.8))
                .child(Icon::new(if open { L::ChevronDown } else { L::ChevronRight }).size(px(14.)))
                .child(t!("Gelişmiş öznitelikler"))
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.advanced_open = !open;
                    cx.notify();
                }))
                .into_any_element(),
        );
        if open {
            rows.push(flag("arch", if dir { t!("Klasör arşivlenmeye hazır") } else { t!("Dosya arşivlenmeye hazır") }, a.archive, |a| &mut a.archive, cx));
            rows.push(flag(
                "idx",
                if dir { t!("Bu klasördeki dosyaların içeriğinin dizinlenmesine izin ver") } else { t!("Bu dosyanın içeriğinin dizinlenmesine izin ver") },
                a.indexed,
                |a| &mut a.indexed,
                cx,
            ));
            if self.caps.0 {
                rows.push(flag("comp", t!("Disk alanından tasarruf etmek için içeriği sıkıştır"), a.compressed, |a| &mut a.compressed, cx));
            }
            if self.caps.1 {
                rows.push(flag("enc", t!("Verileri korumak için içeriği şifrele"), a.encrypted, |a| &mut a.encrypted, cx));
            }
        }
        if dir && self.attrs.changed() {
            let on = self.recurse;
            rows.push(check_row(p, "recurse", t!("Değişiklikleri alt klasörlere ve dosyalara da uygula"), Some(on), cx.listener(move |v, _, _, cx| {
                v.recurse = !on;
                cx.notify();
            })));
        }
        card(p, rows)
    }

    fn render_drive(&self, d: &rf_win::drives::Drive, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use rf_win::drives::DriveKind;
        let kind = match d.kind {
            DriveKind::Removable => t!("Çıkarılabilir disk"),
            DriveKind::Network => t!("Ağ sürücüsü"),
            DriveKind::Optical => t!("CD/DVD sürücüsü"),
            _ => t!("Yerel Disk"),
        };
        let used = d.total.saturating_sub(d.free);
        let frac = d.used_fraction().clamp(0.0, 1.0);
        let bytes = |n: u64| format!("{} ({} {})", format::size(n), group(n), t!("bayt"));
        let swatch = |c: Hsla| div().size(px(12.)).rounded(px(3.)).bg(c).flex_shrink_0();
        let full = frac > 0.9;
        let used_color = if full { p.danger } else { p.accent };
        let mut out = vec![card(
            p,
            vec![
                row(p, t!("Tür"), kind.to_string()),
                row(p, t!("Dosya sistemi"), if d.fs.is_empty() { "—".into() } else { d.fs.clone() }),
                row(p, t!("Etiket"), if d.label.is_empty() { "—".into() } else { d.label.clone() }),
            ],
        )];
        out.push(card(
            p,
            vec![
                h_flex().gap_3().py(px(7.)).text_sm().items_center().child(swatch(used_color)).child(div().w(px(120.)).text_color(p.text_secondary).child(t!("Kullanılan alan"))).child(bytes(used)).into_any_element(),
                h_flex().gap_3().py(px(7.)).text_sm().items_center().child(swatch(p.hover)).child(div().w(px(120.)).text_color(p.text_secondary).child(t!("Boş alan"))).child(bytes(d.free)).into_any_element(),
                div().h(px(1.)).my_1().bg(p.divider).into_any_element(),
                h_flex().gap_3().py(px(7.)).text_sm().items_center().child(div().size(px(12.))).child(div().w(px(120.)).text_color(p.text_secondary).child(t!("Kapasite"))).child(bytes(d.total)).into_any_element(),
                // how full: a bar like the sidebar's, larger
                div()
                    .mt_2()
                    .mb_1()
                    .h(px(14.))
                    .w_full()
                    .rounded(px(7.))
                    .bg(p.hover)
                    .border_1()
                    .border_color(p.border)
                    .child(div().h_full().rounded(px(7.)).bg(used_color).w(relative(frac)))
                    .into_any_element(),
                div().text_xs().text_color(p.text_secondary).child(format!("{} %{:.0}", t!("Dolu:"), frac * 100.0)).into_any_element(),
            ],
        ));
        if d.kind != DriveKind::Network && d.kind != DriveKind::Optical {
            let letter = d.letter;
            out.push(
                link_button(p, "cleanup", L::Eraser, t!("Disk Temizleme…"))
                    .on_click(move |_, _, _| {
                        // cleanmgr runs as the user; only its "system files" button asks for more
                        let _ = std::process::Command::new("cleanmgr.exe").arg(format!("/d{letter}")).spawn();
                    })
                    .into_any_element(),
            );
        }
        out.push(
            link_button(p, "win-props", L::ExternalLink, t!("Windows özellikleri (araçlar, donanım, kota)…"))
                .on_click(cx.listener(|v, _, _, _| v.windows_properties()))
                .into_any_element(),
        );
        out
    }

    fn render_shortcut(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (Some(d), Some(inp)) = (&self.link, &self.link_inputs) else { return vec![] };
        let target = PathBuf::from(rf_win::localized::expand_env(&d.target));
        let target_type = if d.target.is_empty() {
            t!("Kabuk öğesi").to_string()
        } else if target.is_dir() {
            t!("Dosya klasörü").to_string()
        } else {
            match ext_of(&target).as_str() {
                "exe" | "com" | "bat" | "cmd" => t!("Uygulama").to_string(),
                "" => t!("Dosya").to_string(),
                e => format!("{} (.{e})", t!("Dosya")),
            }
        };
        let target_dir = target.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let field = |label: &'static str, input: &Entity<InputState>| {
            h_flex()
                .w_full()
                .gap_3()
                .items_center()
                .py(px(4.))
                .text_sm()
                .child(div().w(px(130.)).flex_shrink_0().text_color(p.text_secondary).child(label))
                .child(text_field(p, input))
                .into_any_element()
        };
        let show = d.show;
        const RUN: [(i32, &str); 3] = [(1, "Normal pencere"), (7, "Simge durumunda"), (3, "Ekranı kaplamış")];
        let current = RUN.iter().find(|(v, _)| *v == show).map_or(RUN[0].1, |(_, l)| l);
        let me = cx.entity().downgrade();
        let run_box = select_box("lnk-run", p, crate::i18n::t(current), move |_| {
            RUN.iter()
                .map(|&(value, label)| {
                    let me = me.clone();
                    Choice::new(crate::i18n::t(label), value == show || (value == 1 && !matches!(show, 3 | 7)), move |_, cx| {
                        let _ = me.update(cx, |v, cx| {
                            if let Some(d) = &mut v.link {
                                d.show = value;
                            }
                            cx.notify();
                        });
                    })
                })
                .collect()
        });
        let admin = d.run_as_admin;
        let icon_start = if d.icon.is_empty() { (d.target.clone(), 0) } else { (d.icon.clone(), d.icon_index) };
        let icon_text = if d.icon.is_empty() { t!("Hedefin simgesi").to_string() } else { format!("{}, {}", file_name(Path::new(&d.icon)), d.icon_index) };
        let hotkey = rf_win::shortcut::hotkey_text(d.hotkey);
        let t2 = target.clone();
        vec![
            card(
                p,
                vec![
                    row(p, t!("Hedef türü"), target_type),
                    row(p, t!("Hedef konumu"), if target_dir.is_empty() { "—".into() } else { target_dir }),
                ],
            ),
            card(
                p,
                vec![
                    field(t!("Hedef"), &inp.target),
                    field(t!("Bağımsız değişkenler"), &inp.args),
                    field(t!("Başlangıç yeri"), &inp.dir),
                    field(t!("Açıklama"), &inp.comment),
                    row(p, t!("Kısayol tuşu"), if hotkey.is_empty() { t!("Yok").to_string() } else { hotkey }),
                    h_flex()
                        .w_full()
                        .gap_3()
                        .items_center()
                        .py(px(6.))
                        .text_sm()
                        .child(div().w(px(130.)).flex_shrink_0().text_color(p.text_secondary).child(t!("Çalıştır")))
                        .child(div().flex_1().min_w_0().child(run_box))
                        .into_any_element(),
                    check_row(p, "lnk-admin", t!("Yönetici olarak çalıştır"), Some(admin), cx.listener(move |v, _, _, cx| {
                        if let Some(d) = &mut v.link {
                            d.run_as_admin = !admin;
                        }
                        cx.notify();
                    })),
                ],
            ),
            card(
                p,
                vec![
                    row(p, t!("Simge"), icon_text),
                    h_flex()
                        .gap_4()
                        .py_1()
                        .child(link_button(p, "lnk-icon", L::Palette, t!("Simge değiştir…")).on_click(cx.listener(move |v, _, window, cx| v.pick_icon(icon_start.clone(), true, window, cx))))
                        .child(link_button(p, "lnk-open", L::FolderOpen, t!("Dosya konumunu aç")).on_click(move |_, _, cx| {
                            let dir = t2.parent().map(Path::to_path_buf).unwrap_or_else(|| t2.clone());
                            crate::workspace::route_ipc(dir.to_string_lossy().into_owned(), cx);
                        }))
                        .into_any_element(),
                ],
            ),
        ]
    }

    fn render_compat(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use rf_win::compat::{Color, Dpi, Mode};
        let Some(exe) = &self.compat_exe else { return vec![] };
        let c = &self.compat;
        let me = cx.entity().downgrade();
        let set = move |f: Box<dyn Fn(&mut Compat)>| {
            let me = me.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = me.update(cx, |v, cx| {
                    f(&mut v.compat);
                    cx.notify();
                });
            }
        };
        let toggle = |id: &'static str, label: &'static str, on: bool, f: fn(&mut Compat), cx: &mut Context<Self>| {
            check_row(p, id, label, Some(on), cx.listener(move |v, _, _, cx| {
                f(&mut v.compat);
                cx.notify();
            }))
        };
        let mode = c.mode;
        let mode_box = {
            let set = set.clone();
            select_box("compat-mode", p, mode.unwrap_or(Mode::Win8).label(), move |_| {
                Mode::ALL.iter().map(|m| {
                    let m = *m;
                    Choice::new(m.label(), mode == Some(m), set(Box::new(move |c| c.mode = Some(m))))
                }).collect()
            })
        };
        let color = c.color;
        let color_box = {
            let set = set.clone();
            let label = |c: Color| match c {
                Color::C256 => t!("8 bit (256) renk"),
                Color::C16Bit => t!("16 bit (65536) renk"),
            };
            select_box("compat-color", p, label(color.unwrap_or(Color::C256)), move |_| {
                [Color::C256, Color::C16Bit].iter().map(|k| {
                    let k = *k;
                    Choice::new(label(k), color == Some(k), set(Box::new(move |c| c.color = Some(k))))
                }).collect()
            })
        };
        let dpi = c.dpi;
        let dpi_box = {
            let set = set.clone();
            let label = |d: Dpi| match d {
                Dpi::Application => t!("Uygulama"),
                Dpi::System => t!("Sistem"),
                Dpi::SystemEnhanced => t!("Sistem (Gelişmiş)"),
            };
            select_box("compat-dpi", p, label(dpi.unwrap_or(Dpi::Application)), move |_| {
                [Dpi::Application, Dpi::System, Dpi::SystemEnhanced].iter().map(|k| {
                    let k = *k;
                    Choice::new(label(k), dpi == Some(k), set(Box::new(move |c| c.dpi = Some(k))))
                }).collect()
            })
        };
        let indent = |e: AnyElement, on: bool| div().pl(px(30.)).pb_1().when(!on, |d| d.opacity(0.5)).child(e).into_any_element();
        let mut out = Vec::new();
        if self.link.is_some() {
            out.push(note(p, &format!("{} {}", t!("Bu ayarlar kısayolun hedefi için geçerlidir:"), breakable(&exe.to_string_lossy()))));
        }
        out.push(card(
            p,
            vec![
                section(p, t!("Uyumluluk modu")),
                toggle("c-mode", t!("Bu programı şunun için uyumluluk modunda çalıştır:"), mode.is_some(), |c| c.mode = if c.mode.is_some() { None } else { Some(Mode::Win8) }, cx),
                indent(mode_box, mode.is_some()),
            ],
        ));
        out.push(card(
            p,
            vec![
                section(p, t!("Ayarlar")),
                toggle("c-color", t!("Azaltılmış renk modu"), color.is_some(), |c| c.color = if c.color.is_some() { None } else { Some(Color::C256) }, cx),
                indent(color_box, color.is_some()),
                toggle("c-640", t!("640 × 480 ekran çözünürlüğünde çalıştır"), c.res_640, |c| c.res_640 = !c.res_640, cx),
                toggle("c-fso", t!("Tam ekran iyileştirmelerini devre dışı bırak"), c.disable_fso, |c| c.disable_fso = !c.disable_fso, cx),
                toggle("c-admin", t!("Bu programı yönetici olarak çalıştır"), c.run_as_admin, |c| c.run_as_admin = !c.run_as_admin, cx),
                toggle("c-restart", t!("Bu programı yeniden başlatma için kaydet"), c.register_restart, |c| c.register_restart = !c.register_restart, cx),
                toggle("c-icc", t!("Eski görüntü ICC renk yönetimini kullan"), c.legacy_icc, |c| c.legacy_icc = !c.legacy_icc, cx),
            ],
        ));
        out.push(card(
            p,
            vec![
                section(p, t!("Yüksek DPI")),
                toggle("c-dpi", t!("Yüksek DPI ölçeklendirme davranışını geçersiz kıl. Ölçeklendirmeyi yapan:"), dpi.is_some(), |c| c.dpi = if c.dpi.is_some() { None } else { Some(Dpi::Application) }, cx),
                indent(dpi_box, dpi.is_some()),
            ],
        ));
        out.push(note(
            p,
            if self.compat_all_users {
                t!("Ayarlar yalnızca sizin hesabınız için kaydedilir. Bu program için tüm kullanıcılara ait ayarlar da var; onları Windows özelliklerinden değiştirebilirsiniz.")
            } else {
                t!("Ayarlar yalnızca sizin hesabınız için kaydedilir.")
            },
        ));
        out
    }

    fn render_security(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use rf_win::security::{RIGHTS, Right};
        let s = match &self.security {
            None => return vec![note(p, t!("Yükleniyor…"))],
            Some(Err(e)) => return vec![card(p, vec![row(p, t!("Hata"), e.clone())]), self.win_props_link(p, cx)],
            Some(Ok(s)) => s,
        };
        let is_dir = self.is_dir();
        let mut out = vec![card(
            p,
            vec![
                row(p, t!("Nesne adı"), breakable(&self.paths[0].to_string_lossy())),
                row(p, t!("Sahip"), s.owner.clone().unwrap_or_else(|| "—".into())),
            ],
        )];
        if s.null_dacl {
            out.push(note(p, t!("Bu öğenin izin listesi yok: herkes her şeyi yapabilir.")));
            out.push(self.win_props_link(p, cx));
            return out;
        }
        let sel = self.sec_sel.min(s.entries.len().saturating_sub(1));
        let mut list = vec![section(p, t!("Grup veya kullanıcı adları"))];
        for (k, e) in s.entries.iter().enumerate() {
            let on = k == sel;
            let group = e.sid.starts_with("S-1-5-32-") || e.sid == "S-1-1-0" || e.sid == "S-1-5-11" || e.sid.starts_with("S-1-15-");
            list.push(
                h_flex()
                    .id(("ace", k))
                    .gap_2()
                    .px_2()
                    .py(px(5.))
                    .rounded(px(4.))
                    .items_center()
                    .text_sm()
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.hover))
                    .hover(|d| d.bg(p.hover))
                    .child(Icon::new(if group { L::Users } else { L::User }).size(px(14.)).text_color(p.text_secondary))
                    .child(div().flex_1().min_w_0().truncate().child(e.name.clone()))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.sec_sel = k;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        out.push(card(p, list));
        if let Some(e) = s.entries.get(sel) {
            let allow = rf_win::security::ticks(e.allow, is_dir);
            let deny = rf_win::security::ticks(e.deny, is_dir);
            let mark = |on: bool| {
                div()
                    .w(px(70.))
                    .flex()
                    .justify_center()
                    .children(on.then(|| Icon::new(L::Check).size(px(14.)).text_color(p.accent)))
            };
            let mut rows = vec![
                h_flex()
                    .py(px(5.))
                    .text_xs()
                    .text_color(p.text_secondary)
                    .child(div().flex_1().min_w_0().truncate().child(format!("{} {}", e.name, t!("için izinler"))))
                    .child(div().w(px(70.)).flex().justify_center().child(t!("İzin ver")))
                    .child(div().w(px(70.)).flex().justify_center().child(t!("Reddet")))
                    .into_any_element(),
            ];
            for r in RIGHTS {
                if r == Right::List && !is_dir {
                    continue;
                }
                let label = match r {
                    Right::Full => t!("Tam denetim"),
                    Right::Modify => t!("Değiştirme"),
                    Right::ReadExecute => t!("Okuma ve yürütme"),
                    Right::List => t!("Klasör içeriğini listeleme"),
                    Right::Read => t!("Okuma"),
                    Right::Write => t!("Yazma"),
                    Right::Special => t!("Özel izinler"),
                };
                rows.push(
                    h_flex()
                        .py(px(5.))
                        .text_sm()
                        .items_center()
                        .child(div().flex_1().child(label))
                        .child(mark(allow.contains(&r)))
                        .child(mark(deny.contains(&r)))
                        .into_any_element(),
                );
            }
            out.push(card(p, rows));
        }
        if s.protected {
            out.push(note(p, t!("Bu öğe izinlerini üst klasörden devralmıyor.")));
        }
        out.push(note(p, t!("İzinleri değiştirmek için Windows'un Güvenlik penceresini kullanın (yönetici izni isteyebilir).")));
        out.push(self.win_props_link(p, cx));
        out
    }

    fn win_props_link(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        link_button(p, "win-props2", L::ExternalLink, t!("Windows özellikleri…")).on_click(cx.listener(|v, _, _, _| v.windows_properties())).into_any_element()
    }

    fn render_details(&self, p: &Palette) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(pe) = &self.pe {
            let mut rows = vec![section(p, t!("Program"))];
            let mut add = |k: &'static str, v: &Option<String>| {
                if let Some(v) = v.as_ref().filter(|v| !v.trim().is_empty()) {
                    rows.push(row(p, k, v.clone()));
                }
            };
            add(t!("Dosya açıklaması"), &pe.description);
            add(t!("Dosya sürümü"), &pe.version);
            add(t!("Ürün adı"), &pe.product);
            add(t!("Ürün sürümü"), &pe.product_version);
            add(t!("Şirket"), &pe.company);
            add(t!("Telif hakkı"), &pe.copyright);
            add(t!("Ticari markalar"), &pe.trademarks);
            add(t!("Dil"), &pe.language);
            add(t!("Özgün dosya adı"), &pe.original_filename);
            add(t!("İç ad"), &pe.internal_name);
            rows.push(row(p, t!("Mimari"), pe.arch.to_string()));
            out.push(card(p, rows));
        }
        if !self.media.is_empty() {
            let mut rows = vec![section(p, t!("İçerik"))];
            rows.extend(self.media.iter().map(|(k, v)| row(p, crate::i18n::t(k), v.clone())));
            out.push(card(p, rows));
        }
        if let (Some(path), Some(Ok(i))) = (self.single(), &self.info) {
            let mut letters = String::new();
            for (bit, c) in [(READONLY, 'R'), (HIDDEN, 'H'), (0x4, 'S'), (ARCHIVE, 'A'), (COMPRESSED, 'C'), (ENCRYPTED, 'E'), (0x400, 'L'), (0x1000, 'O')] {
                if i.attrs & bit != 0 {
                    letters.push(c);
                }
            }
            let mut rows = vec![
                section(p, t!("Dosya")),
                row(p, t!("Ad"), file_name(path)),
                row(p, t!("Klasör yolu"), path.parent().map(|d| breakable(&d.to_string_lossy())).unwrap_or_default()),
                row(p, t!("Oluşturulma"), format::datetime_full(i.created_ms)),
                row(p, t!("Değiştirilme"), format::datetime_full(i.modified_ms)),
                row(p, t!("Boyut"), format::size(i.size)),
                row(p, t!("Öznitelikler"), if letters.is_empty() { "—".into() } else { letters }),
            ];
            if let Some(Ok(s)) = &self.security {
                if let Some(o) = &s.owner {
                    rows.push(row(p, t!("Sahip"), o.clone()));
                }
            }
            if let Ok(pc) = std::env::var("COMPUTERNAME") {
                rows.push(row(p, t!("Bilgisayar"), format!("{pc} ({})", t!("bu bilgisayar"))));
            }
            out.push(card(p, rows));
        }
        out
    }

    fn render_signatures(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(s) = &self.signatures else { return vec![] };
        let mut out = Vec::new();
        let (ok_icon, color, text) = match &s.verdict {
            Ok(()) => (L::ShieldCheck, hsla(140. / 360., 0.6, 0.45, 1.), t!("Bu dijital imza geçerli.").to_string()),
            Err(e) => (L::ShieldAlert, p.danger, format!("{} ({})", t!("Bu dijital imza geçerli değil."), verdict_reason(e))),
        };
        out.push(
            h_flex()
                .gap_2()
                .px_4()
                .py_3()
                .rounded(theme::RADIUS)
                .bg(p.card_bg)
                .border_1()
                .border_color(p.border)
                .items_center()
                .text_sm()
                .child(Icon::new(ok_icon).size(px(20.)).text_color(color))
                .child(div().flex_1().child(text))
                .into_any_element(),
        );
        let sel = self.sig_sel.min(s.list.len().saturating_sub(1));
        let mut list = vec![
            h_flex()
                .py(px(5.))
                .text_xs()
                .text_color(p.text_secondary)
                .child(div().flex_1().child(t!("İmzalayan")))
                .child(div().w(px(70.)).child(t!("Özet")))
                .child(div().w(px(130.)).child(t!("Zaman damgası")))
                .into_any_element(),
        ];
        for (k, g) in s.list.iter().enumerate() {
            let on = k == sel;
            list.push(
                h_flex()
                    .id(("sig", k))
                    .px_1()
                    .py(px(5.))
                    .rounded(px(4.))
                    .text_sm()
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.hover))
                    .hover(|d| d.bg(p.hover))
                    .child(div().flex_1().min_w_0().truncate().child(g.signer.clone()))
                    .child(div().w(px(70.)).child(g.digest.clone()))
                    .child(div().w(px(130.)).child(g.timestamp_ms.map(format::datetime_full).unwrap_or_else(|| t!("Yok").to_string())))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.sig_sel = k;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        out.push(card(p, list));
        if let Some(g) = s.list.get(sel) {
            out.push(card(
                p,
                vec![
                    section(p, t!("İmza ayrıntıları")),
                    row(p, t!("İmzalayan"), g.signer.clone()),
                    row(p, t!("Veren"), g.issuer.clone()),
                    row(p, t!("Özet algoritması"), g.digest.clone()),
                    row(p, t!("İmzalanma zamanı"), g.timestamp_ms.map(format::datetime_full).unwrap_or_else(|| t!("Zaman damgası yok").to_string())),
                ],
            ));
        }
        out
    }

    fn render_sharing(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = vec![section(p, t!("Ağ dosya ve klasör paylaşımı"))];
        match &self.share {
            Some(s) => {
                rows.push(row(p, t!("Durum"), t!("Paylaşılıyor").to_string()));
                rows.push(row(p, t!("Ağ yolu"), s.clone()));
            }
            None => rows.push(row(p, t!("Durum"), t!("Paylaşılmıyor").to_string())),
        }
        vec![
            card(p, rows),
            note(p, t!("Paylaşımı açmak ya da değiştirmek yönetici izni gerektirir; Windows'un Paylaşım penceresinden yapılır.")),
            self.win_props_link(p, cx),
        ]
    }

    fn render_customize(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let current = match &self.folder_icon {
            Some((f, i)) => format!("{}, {i}", file_name(Path::new(f))),
            None => t!("Varsayılan").to_string(),
        };
        let start = self.folder_icon.clone().unwrap_or_else(|| (r"%SystemRoot%\System32\shell32.dll".to_string(), 3));
        let custom = self.folder_icon.is_some();
        vec![
            card(
                p,
                vec![
                    section(p, t!("Klasör simgesi")),
                    row(p, t!("Simge"), current),
                    h_flex()
                        .gap_4()
                        .py_1()
                        .child(link_button(p, "dir-icon", L::Palette, t!("Simge değiştir…")).on_click(cx.listener(move |v, _, window, cx| v.pick_icon(start.clone(), false, window, cx))))
                        .when(custom, |d| {
                            d.child(link_button(p, "dir-icon-reset", L::RotateCcw, t!("Varsayılana geri yükle")).on_click(cx.listener(|v, _, _, cx| {
                                v.folder_icon = None;
                                cx.notify();
                            })))
                        })
                        .into_any_element(),
                ],
            ),
            note(p, t!("Simge, klasöre bir desktop.ini dosyası yazılarak ayarlanır; Dosya Gezgini de aynı simgeyi gösterir.")),
        ]
    }
}

/// Everything the window shows about one item, read on a background thread.
fn load_one(p: &Path) -> Loaded {
    let mut l = Loaded::default();
    let info = rf_win::fileinfo::info(p).map_err(|e| e.to_string());
    let is_dir = info.as_ref().is_ok_and(|i| i.is_dir);
    let is_file = info.as_ref().is_ok_and(|i| !i.is_dir);
    l.blocked = is_file && rf_win::fileinfo::zone_blocked(p);
    l.caps = rf_win::fileinfo::volume_caps(p);
    if is_dir && is_root(p) {
        let letter = p.to_string_lossy().chars().next().unwrap_or('C').to_ascii_uppercase();
        l.drive = rf_win::drives::list_drives().into_iter().find(|d| d.letter.to_ascii_uppercase() == letter);
    }
    if is_dir {
        l.share = rf_win::fileinfo::share_of(p);
        if l.drive.is_none() {
            l.folder_icon = rf_win::folder_custom::folder_icon(p);
        }
    }
    if is_file {
        if is_pe(p) {
            l.pe = rf_win::pe::info(p);
        }
        if signable(p) {
            l.signatures = rf_win::signature::read(p);
        }
        if ext_of(p) == "lnk" {
            l.link = rf_win::shortcut::details(p).ok();
        }
        // the program the Compatibility tab is about
        let exe = match &l.link {
            Some(d) => Some(PathBuf::from(rf_win::localized::expand_env(&d.target))),
            None => Some(p.to_path_buf()),
        };
        l.compat_exe = exe.filter(|e| ext_of(e) == "exe");
        if let Some(e) = &l.compat_exe {
            l.compat = rf_win::compat::read(e);
            l.compat_all_users = rf_win::compat::read_all_users(e) != Compat::default();
        }
    }
    l.info = Some(info);
    l
}

/// WinVerifyTrust's reason (English, from rf_win::signature) in the UI language.
fn verdict_reason(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    let k = if l.contains("does not match") {
        t!("imza dosyayla uyuşmuyor")
    } else if l.contains("distrust") {
        t!("sertifika açıkça engellenmiş")
    } else if l.contains("not trusted") {
        t!("sertifikaya güvenilmiyor")
    } else if l.contains("expired") {
        t!("sertifikanın süresi dolmuş")
    } else if l.contains("chain") {
        t!("sertifika zinciri kurulamadı")
    } else if l.contains("no signature") {
        t!("imza yok")
    } else if l.contains("revoked") {
        t!("sertifika iptal edilmiş")
    } else if l.contains("code signing") {
        t!("sertifika kod imzalamak için geçerli değil")
    } else {
        return e.to_string();
    };
    k.to_string()
}

/// A folder and everything under it (not following junctions or links), for a change that
/// goes down the tree.
fn walk(dir: &Path, cancel: &AtomicBool) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                // a junction is a directory with a reparse point
                use std::os::windows::fs::MetadataExt;
                if e.metadata().is_ok_and(|m| m.file_attributes() & 0x400 != 0) {
                    continue;
                }
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}

fn row(p: &Palette, k: &'static str, v: String) -> AnyElement {
    h_flex()
        .w_full()
        .gap_3()
        .items_start()
        .py(px(7.))
        .text_sm()
        .child(div().w(px(150.)).flex_shrink_0().text_color(p.text_secondary).child(k))
        .child(div().flex_1().min_w_0().child(v))
        .into_any_element()
}

/// A row with a small action on its right ("Change…").
fn row_with(p: &Palette, k: &'static str, v: String, action: AnyElement) -> AnyElement {
    h_flex()
        .w_full()
        .gap_3()
        .items_center()
        .py(px(5.))
        .text_sm()
        .child(div().w(px(150.)).flex_shrink_0().text_color(p.text_secondary).child(k))
        .child(div().flex_1().min_w_0().child(v))
        .child(action)
        .into_any_element()
}

fn section(p: &Palette, title: &'static str) -> AnyElement {
    div().pt_1().pb(px(2.)).text_xs().font_weight(FontWeight::SEMIBOLD).text_color(p.text_secondary).child(title).into_any_element()
}

fn note(p: &Palette, text: &str) -> AnyElement {
    div().text_xs().text_color(p.text_secondary).child(text.to_string()).into_any_element()
}

fn link_button(p: &Palette, id: &'static str, icon: L, label: &'static str) -> Stateful<Div> {
    h_flex()
        .id(id)
        .gap_1p5()
        .items_center()
        .flex_shrink_0()
        .text_sm()
        .text_color(p.accent)
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child(Icon::new(icon).size(px(14.)))
        .child(label)
}

fn text_field(p: &Palette, input: &Entity<InputState>) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .bg(p.content_bg)
        .border_1()
        .border_color(p.border)
        .child(Input::new(input).appearance(false))
        .into_any_element()
}

fn card(p: &Palette, rows: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .px_4()
        .py_2()
        .rounded(theme::RADIUS)
        .bg(p.card_bg)
        .border_1()
        .border_color(p.border)
        .children(rows)
        .into_any_element()
}

fn check_row(
    p: &Palette,
    id: &'static str,
    label: &'static str,
    on: Option<bool>,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (fill, mark) = match on {
        Some(true) => (Some(p.accent), Some(L::Check)),
        None => (Some(p.accent), Some(L::Minus)),
        Some(false) => (None, None),
    };
    h_flex()
        .id(id)
        .gap_3()
        .py(px(7.))
        .items_center()
        .text_sm()
        .cursor_pointer()
        .child(
            div()
                .size(px(18.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .when_some(fill, |d, c| d.bg(c))
                .when(fill.is_none(), |d| d.border_1().border_color(p.text_secondary))
                .children(mark.map(|m| Icon::new(m).size(px(12.)).text_color(p.accent_text))),
        )
        .child(div().flex_1().min_w_0().child(label))
        .on_click(click)
        .into_any_element()
}

impl Render for PropsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        if self.frame_dark != Some(p.dark) {
            self.frame_dark = Some(p.dark);
            let h = Self::hwnd(window);
            if h != 0 {
                rf_win::window::set_dark_mode(h, p.dark);
            }
        }
        let title = match (self.single(), &self.drive) {
            (_, Some(d)) => format!("{} — {}", d.display_name(), t!("Özellikler")),
            (Some(path), None) => format!("{} — {}", file_name(path), t!("Özellikler")),
            (None, None) => format!("{} — {}", crate::i18n::items(self.paths.len()), t!("Özellikler")),
        };
        window.set_window_title(&title);

        // header: big icon / thumbnail + name
        let icon: AnyElement = match (self.entry(), &self.drive) {
            (_, Some(_)) => Icon::new(L::HardDrive).size(px(40.)).text_color(p.text_secondary).into_any_element(),
            (Some(e), None) => {
                let dir = self.paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
                match self.images.update(cx, |c, cx| c.thumbnail(&dir, &e, 96, cx)) {
                    Some(im) => img(im).size(px(48.)).object_fit(ObjectFit::Contain).into_any_element(),
                    None => {
                        let cat = crate::icons::category(&e);
                        Icon::new(crate::icons::icon_for(cat)).size(px(40.)).text_color(theme::kind_color(cat, p.dark)).into_any_element()
                    }
                }
            }
            (None, None) => Icon::new(if self.paths.len() > 1 { L::Files } else { L::File }).size(px(40.)).text_color(p.text_secondary).into_any_element(),
        };
        let name: AnyElement = match (&self.name, &self.drive) {
            (_, Some(d)) => div().flex_1().text_base().font_weight(FontWeight::SEMIBOLD).child(d.display_name()).into_any_element(),
            (Some(n), None) => text_field(&p, n),
            (None, None) => div().flex_1().text_base().font_weight(FontWeight::SEMIBOLD).child(crate::i18n::items(self.paths.len())).into_any_element(),
        };

        // tabs (they wrap onto a second row when many, as Windows' do)
        let pages = self.pages();
        if !pages.iter().any(|(pg, _)| *pg == self.page) {
            self.page = Page::General;
        }
        let mut tabs = h_flex().flex_wrap().p(px(2.)).gap(px(2.)).rounded(theme::RADIUS_SM).bg(p.content_bg).border_1().border_color(p.border);
        let show_tabs = pages.len() > 1;
        for (pg, label) in pages {
            let on = pg == self.page;
            tabs = tabs.child(
                div()
                    .id(label)
                    .flex_grow(1.)
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .text_sm()
                    .when(on, |d| d.bg(p.card_bg).shadow_sm().font_weight(FontWeight::MEDIUM))
                    .when(!on, |d| d.text_color(p.text_secondary).hover(|s| s.bg(p.hover)))
                    .child(label)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.page = pg;
                        match pg {
                            Page::Hashes if v.hash.is_none() => {
                                let path = v.paths[0].clone();
                                v.hash = Some(cx.new(|cx| crate::hash_panel::HashPanel::new(path, cx)));
                            }
                            Page::Security | Page::Details => v.load_security(cx),
                            _ => {}
                        }
                        cx.notify();
                    })),
            );
        }

        let body: Vec<AnyElement> = match self.page {
            Page::General => self.render_general(&p, cx),
            Page::Shortcut => self.render_shortcut(&p, cx),
            Page::Compat => self.render_compat(&p, cx),
            Page::Security => self.render_security(&p, cx),
            Page::Details => self.render_details(&p),
            Page::Signatures => self.render_signatures(&p, cx),
            Page::Sharing => self.render_sharing(&p, cx),
            Page::Customize => self.render_customize(&p, cx),
            Page::Hashes => self.hash.clone().map(|h| vec![card(&p, vec![div().py_2().child(h).into_any_element()])]).unwrap_or_default(),
        };

        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .min_w(px(88.))
                .h(px(32.))
                .px_4()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .when(primary, |d| d.bg(p.accent).text_color(p.accent_text))
                .when(!primary, |d| d.bg(p.hover).border_1().border_color(p.border))
                .hover(|s| s.opacity(0.9))
                .active(|s| s.opacity(0.8))
                .child(label)
        };
        let dirty = self.dirty(cx);

        v_flex()
            .size_full()
            .bg(p.window_bg)
            .text_color(p.text)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| match ev.keystroke.key.as_str() {
                "escape" => window.remove_window(),
                "enter" => v.apply(true, window, cx),
                _ => {}
            }))
            .child(
                TitleBar::new()
                    .h(theme::TITLEBAR_H)
                    .bg(p.titlebar_bg)
                    .border_color(gpui_kit::transparent_black())
                    .child(h_flex().gap_2().items_center().text_xs().child(Icon::new(L::Info).size(px(14.))).child(title.clone())),
            )
            .child(
                v_flex()
                    .id("props-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .pt_2()
                    .pb_4()
                    .gap_4()
                    .child(h_flex().gap_3().items_center().child(div().size(px(48.)).flex().items_center().justify_center().child(icon)).child(name))
                    .when(show_tabs, |d| d.child(tabs))
                    .children(body),
            )
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(p.divider)
                    .child(div().flex_1().min_w_0().text_xs().text_color(p.danger).children(self.error.clone()))
                    .child(button("ok", t!("Tamam"), true).on_click(cx.listener(|v, _, window, cx| v.apply(true, window, cx))))
                    .child(button("cancel", t!("İptal"), false).on_click(|_, window, _| window.remove_window()))
                    .child(
                        button("apply", t!("Uygula"), false)
                            .when(!dirty, |d| d.opacity(0.5))
                            .on_click(cx.listener(|v, _, window, cx| v.apply(false, window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ARCHIVE, AtomicBool, NOT_INDEXED, Path, READONLY, attrs_from, is_pe, is_root, merge_attrs, signable, walk};

    #[test]
    fn attribute_tristates_merge() {
        let mut acc = [None; 6];
        merge_attrs(&mut acc, READONLY | ARCHIVE, true);
        merge_attrs(&mut acc, ARCHIVE | NOT_INDEXED, false);
        // read-only mixed, hidden off, archive on, indexed mixed
        assert_eq!(&acc[..4], &[None, Some(false), Some(true), None]);
        let mut a = attrs_from(acc);
        assert!(!a.changed());
        a.readonly.toggle();
        assert!(a.changed());
        a.commit();
        assert!(!a.changed());
    }

    #[test]
    fn kinds_of_files() {
        assert!(is_pe(Path::new(r"C:\a\b.EXE")) && is_pe(Path::new("x.dll")) && !is_pe(Path::new("x.txt")));
        assert!(signable(Path::new("kur.msi")) && signable(Path::new("a.ps1")) && !signable(Path::new("a.png")));
        assert!(is_root(Path::new(r"C:\")) && !is_root(Path::new(r"C:\Windows")));
    }

    #[test]
    fn walking_a_tree() {
        let d = std::env::temp_dir().join(format!("rf-props-walk-{}", std::process::id()));
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/x.txt"), b"x").unwrap();
        std::fs::write(d.join("y.txt"), b"y").unwrap();
        let all = walk(&d, &AtomicBool::new(false));
        assert_eq!(all.len(), 5, "{all:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
