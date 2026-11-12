//! Window chrome: title bar with tabs, toolbar (nav + breadcrumb/address + search),
//! sidebar, active tab content, status bar.

use crate::t;
use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::{Location, format};

use crate::actions::*;
use crate::folder_view::{FolderEvent, FolderView, ViewMode};
use crate::image_cache::ImageCache;
use crate::preview::Preview;
use crate::places::{self, Places};
use crate::theme::{self, Palette};

/// Payload while a pinned sidebar entry is dragged (reordering).
#[derive(Clone)]
struct PinDrag {
    ix: usize,
    /// Identity of the dragged pin (indices of the drawn list can be stale).
    path: PathBuf,
    title: SharedString,
}

impl Render for PinDrag {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        div()
            .px_3()
            .py_1p5()
            .rounded(theme::RADIUS)
            .bg(p.card_bg)
            .border_1()
            .border_color(p.accent)
            .text_sm()
            .text_color(p.text)
            .shadow_md()
            .child(self.title.clone())
    }
}

/// Payload while a tab is dragged (reordering).
#[derive(Clone)]
struct TabDrag {
    ix: usize,
    title: SharedString,
}

impl Render for TabDrag {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        div()
            .px_3()
            .py_1p5()
            .rounded(theme::RADIUS)
            .bg(p.tab_active)
            .border_1()
            .border_color(p.accent)
            .text_sm()
            .text_color(p.text)
            .shadow_md()
            .child(self.title.clone())
    }
}
#[derive(Clone)]
/// Window placement to go back to when leaving the compact overlay.
struct CompactRestore {
    rect: [i32; 4],
    maximized: bool,
    topmost: bool,
}

/// Compact overlay size and minimum (logical px).
const COMPACT_W: f32 = 420.;
const COMPACT_H: f32 = 560.;
const COMPACT_MIN: (u16, u16) = (340, 300);

/// A tab holds one folder view, or two side by side (dual pane).
#[derive(Clone)]
struct Tab {
    left: Entity<FolderView>,
    right: Option<Entity<FolderView>>,
    right_active: bool,
}

impl Tab {
    /// The pane that has focus (toolbar, sidebar and status bar act on it).
    fn view(&self) -> &Entity<FolderView> {
        match (&self.right, self.right_active) {
            (Some(r), true) => r,
            _ => &self.left,
        }
    }
}

pub struct Workspace {
    tabs: Vec<Tab>,
    /// Compact overlay (small, always-on-top window): the placement to restore on exit.
    compact: Option<CompactRestore>,
    active: usize,
    closed: Vec<Location>,
    places: Entity<Places>,
    images: Entity<ImageCache>,
    ops: Entity<crate::status_center::Ops>,
    preview: Entity<Preview>,
    preview_open: bool,
    /// Space: big preview of the focused item over the file list.
    quick_look: Option<Entity<Preview>>,
    shelf: Entity<crate::shelf_view::ShelfView>,
    shelf_open: bool,
    address: Entity<InputState>,
    editing_address: bool,
    /// The Omnibar is in search mode (Ctrl+F / its search button) instead of showing the path.
    search_mode: bool,
    /// (dark, system appearance) last pushed to the window frame (see render).
    frame_dark: Option<(bool, WindowAppearance)>,
    /// Background material last applied to the window.
    applied_backdrop: Option<crate::settings::BackdropPref>,
    /// Address-bar folder completions for the typed text, and the highlighted one.
    addr_suggest: Vec<PathBuf>,
    addr_sel: Option<usize>,
    _addr_task: Option<Task<()>>,
    search: Entity<InputState>,
    /// Files' "Filter by" strip under the command bar (name filter for this folder).
    filter_bar: bool,
    filter_input: Entity<InputState>,
    sidebar_open: bool,
    sidebar_w: f32,
    /// Icons only (RAIL_W wide); file dialogs start this way.
    sidebar_rail: bool,
    /// The full sidebar shown over the content while the pointer is on the rail (or on it).
    rail_peek: [bool; 2],
    /// The colour the window shows behind the rail (Mica's wallpaper tint), read off the screen
    /// when the full sidebar opens over the content: painted with it, the sidebar looks like the
    /// docked one instead of a neutral grey slab over a tinted window.
    rail_peek_bg: Option<Hsla>,
    preview_w: f32,
    /// Debounced settings write after a pane resize.
    _width_save: Option<Task<()>>,
    palette: Option<Entity<crate::command_palette::CommandPalette>>,
    bulk: Option<Entity<crate::bulk_rename::BulkRename>>,
    /// Last window title / session snapshot we pushed (avoid redundant work every frame).
    last_title: String,
    /// Tab strip scrolling (many tabs): keeps the active tab in view.
    tab_scroll: ScrollHandle,
    tab_scrolled_to: Option<usize>,
    /// Tab being opened with an animation: (its view, a serial so each opening animates anew).
    tab_anim: Option<(Entity<FolderView>, u64)>,
    tab_anim_serial: u64,
    /// Files dragged over an inactive tab: (tab, serial). Held still there, the tab opens.
    drag_hover_tab: Option<(usize, u64)>,
    /// Files are being dragged over the window: the sidebar shows its "drop here to pin" row.
    sidebar_ext_drag: bool,
    /// This window answers another app's Open/Save dialog (see `picker`).
    picker: Option<std::rc::Rc<crate::picker::PickSession>>,
    /// Window width from the last frame (breadcrumb collapsing).
    viewport_w: f32,
    last_session: Vec<String>,
    _bulk_sub: Option<Subscription>,
    _palette_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
}

/// Every open main window's workspace, so app-level events (a second launch) can reach one.
#[derive(Default)]
pub struct OpenWorkspaces(Vec<(AnyWindowHandle, WeakEntity<Workspace>)>);
impl Global for OpenWorkspaces {}

/// A second launch forwarded its argument: open it as a tab in the most recent live window
/// and bring that window up; with no window left, open a new one.
/// Taskbar jump list: "New window" / "New tab" and the pinned folders. Rewritten only when the
/// pins or the language change; test runs (several instances) leave the user's list alone.
fn update_jump_list(places: &crate::places::Places) {
    static LAST: std::sync::Mutex<Option<(Vec<(String, PathBuf)>, [&'static str; 3])>> = std::sync::Mutex::new(None);
    if !places.loaded || std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_some() {
        return;
    }
    // in the UI language, as the sidebar shows them (a default pin's title is its Turkish name)
    let pins: Vec<(String, PathBuf)> = places.pinned.iter().map(|p| (crate::i18n::td(&p.title), p.path.clone())).collect();
    let labels = [t!("Yeni pencere"), t!("Yeni sekme"), t!("Sabitlenenler")];
    {
        let mut last = LAST.lock().unwrap();
        if last.as_ref() == Some(&(pins.clone(), labels)) {
            return;
        }
        *last = Some((pins.clone(), labels));
    }
    let Ok(exe) = std::env::current_exe() else { return };
    std::thread::spawn(move || {
        let tasks = [(labels[0], rf_win::jumplist::NEW_WINDOW_ARG), (labels[1], "")];
        if let Err(e) = rf_win::jumplist::set(&exe, &tasks, labels[2], &pins) {
            log::warn!("jump list: {e:#}");
        }
    });
}

/// Whether `path` lives on drive `letter` (`E:\…`, `\\?\E:\…`).
pub fn on_drive(path: &std::path::Path, letter: char) -> bool {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut c = s.chars();
    matches!((c.next(), c.next()), (Some(l), Some(':')) if l.eq_ignore_ascii_case(&letter))
}

/// Windows asks to eject `letter` ("Safely remove", eject from our sidebar or Explorer): every
/// window lets go of it — panes on that drive go Home (their folder watchers, size walks and
/// listings stop), its previews close, queued thumbnails are dropped.
pub fn release_drive(letter: char, cx: &mut App) {
    let list = cx.try_global::<OpenWorkspaces>().map(|g| g.0.clone()).unwrap_or_default();
    for (handle, ws) in list {
        let Some(ws) = ws.upgrade() else { continue };
        let _ = handle.update(cx, |_, _, cx| ws.update(cx, |w, cx| w.release_drive(letter, cx)));
    }
}

/// Whether any app window (not a menu / flyout popup) is still open.
/// The open main windows (not Settings, DirectLook or popups).
pub fn main_windows(cx: &App) -> Vec<AnyWindowHandle> {
    let open = cx.windows();
    cx.try_global::<OpenWorkspaces>()
        .map(|g| open.into_iter().filter(|w| g.0.iter().any(|(h, ws)| ws.upgrade().is_some() && w.window_id() == h.window_id())).collect())
        .unwrap_or_default()
}

pub fn any_main_window(cx: &App) -> bool {
    let open = cx.windows();
    cx.try_global::<OpenWorkspaces>()
        .is_some_and(|g| g.0.iter().any(|(h, ws)| ws.upgrade().is_some() && open.iter().any(|w| w.window_id() == h.window_id())))
}

/// A second launch's message that asks for a file to be shown selected in its folder
/// (`smoothyfiles /select,<file>`, like Explorer).
pub const SELECT_PREFIX: &str = "\u{1}select:";

/// A file to select in the window that just opened its folder ("Show in folder" from another app,
/// see rf_win::shell_window): selected in place, or its folder opened when no window shows it.
pub const SELECT_HERE_PREFIX: &str = "\u{1}select-here:";

/// A second launch on a shell folder (a phone, Network…) reaches the running window as this
/// prefix plus the folder's parsing name (and, in a saved session, a tab and its name).
pub const SHELL_PREFIX: &str = "smoothyfiles-shell:";

/// How a tab is kept in the saved session: a folder's path, a search's folder, the Recycle Bin
/// and shell folders (a phone…) by name; other places come back as Home ("").
fn session_text(loc: &Location) -> String {
    match loc {
        Location::Path(p) => rf_core::history::redact_credentials(&p.to_string_lossy()),
        Location::Search { root, .. } => root.to_string_lossy().into_owned(),
        Location::RecycleBin => "shell:RecycleBinFolder".into(),
        Location::Shell { parsing, name } => format!("{SHELL_PREFIX}{parsing}\t{name}"),
        _ => String::new(),
    }
}

/// What a launch message opens: a folder (or Home), and maybe a name to select there.
pub fn parse_launch(msg: &str) -> (Location, Option<String>) {
    let m = msg.trim();
    if let Some(rest) = m.strip_prefix(SHELL_PREFIX) {
        let (parsing, name) = rest.split_once('\t').unwrap_or((rest, ""));
        return (Location::Shell { parsing: parsing.to_string(), name: name.to_string() }, None);
    }
    if let Some(loc) = Location::from_shell_name(m) {
        return (loc, None);
    }
    if let Some(file) = m.strip_prefix(SELECT_PREFIX) {
        let p = PathBuf::from(file);
        if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
            return (Location::Path(dir.to_path_buf()), Some(name.to_string_lossy().into_owned()));
        }
    }
    if m.is_empty() { (Location::Home, None) } else { (Location::Path(PathBuf::from(m)), None) }
}

/// Selects `file` in the newest window whose current tab shows its folder (and brings that window
/// forward); false when none does.
fn select_in_open_folder(file: &std::path::Path, cx: &mut App) -> bool {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else { return false };
    let key = |p: &std::path::Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    let want = key(dir);
    let name = name.to_string_lossy().into_owned();
    let list = cx.try_global::<OpenWorkspaces>().map(|g| g.0.clone()).unwrap_or_default();
    for (handle, ws) in list.into_iter().rev() {
        let Some(ws) = ws.upgrade() else { continue };
        let view = ws.read(cx).active_view();
        let here = matches!(view.read(cx).location(), Location::Path(p) if key(p) == want);
        if !here {
            continue;
        }
        let name = name.clone();
        if handle
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| v.select_now(name, cx));
                window.activate_window();
            })
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// The `/select,` file of the first launch, picked up by the first window.
pub static START_SELECT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// DirectLook'a en son gönderilen dosya (seçim değişince yalnız yenisi gider).
static LOOK_FOLLOWED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub fn route_ipc(msg: String, cx: &mut App) {
    if let Some(pipe) = msg.strip_prefix(crate::picker::PICK_PREFIX) {
        crate::picker::begin(pipe.to_string(), cx);
        return;
    }
    match crate::look::ipc::ToApp::decode(&msg) {
        Some(crate::look::ipc::ToApp::LookClosed) => {
            crate::folder_view::QUICK_LOOK_OPEN.store(false, std::sync::atomic::Ordering::Relaxed);
            *LOOK_FOLLOWED.lock().unwrap() = None;
            return;
        }
        Some(crate::look::ipc::ToApp::OpenSettings) => {
            crate::settings_view::open_page(crate::settings_view::Page::Look, cx);
            crate::open_main_window(Some(Location::Settings), None, None, cx);
            return;
        }
        None => {}
    }
    if let Some(file) = msg.strip_prefix(SELECT_HERE_PREFIX) {
        if !select_in_open_folder(std::path::Path::new(file), cx) {
            route_ipc(format!("{SELECT_PREFIX}{file}"), cx);
        }
        return;
    }
    if msg.trim() == rf_win::jumplist::NEW_WINDOW_ARG {
        crate::open_main_window(Some(Location::Home), None, None, cx);
        return;
    }
    if msg.trim() == crate::quick_search::LAUNCH_ARG {
        crate::quick_search::toggle(cx);
        return;
    }
    let (loc, select) = parse_launch(&msg);
    let list = cx.try_global::<OpenWorkspaces>().map(|g| g.0.clone()).unwrap_or_default();
    for (handle, ws) in list.into_iter().rev() {
        let Some(ws) = ws.upgrade() else { continue };
        let (target, sel) = (loc.clone(), select.clone());
        let ok = handle
            .update(cx, |_, window, cx| {
                ws.update(cx, |w, cx| w.open_tab_selecting(target, sel, window, cx));
                window.activate_window();
            })
            .is_ok();
        if ok {
            return;
        }
    }
    *START_SELECT.lock().unwrap() = select;
    crate::open_main_window(Some(loc), None, None, cx);
}

impl Workspace {
    pub fn new(
        start: Option<Location>,
        ipc: Option<futures::channel::mpsc::UnboundedReceiver<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(mut rx) = ipc {
            cx.spawn_in(window, async move |this, cx| {
                use futures::StreamExt;
                while let Some(msg) = rx.next().await {
                    let (loc, select) = parse_launch(&msg);
                    let ok = this
                        .update_in(cx, |ws, window, cx| {
                            ws.open_tab_selecting(loc, select, window, cx);
                            window.activate_window();
                        })
                        .is_ok();
                    if !ok {
                        break;
                    }
                }
            })
            .detach();
        }
        let places = cx.new(Places::new);
        let images = cx.new(ImageCache::new);
        let ops = cx.global::<crate::status_center::OpsGlobal>().0.clone();
        cx.observe(&ops, |_, _, cx| cx.notify()).detach();
        let preview = {
            let images = images.clone();
            cx.new(|cx| Preview::new(images, cx))
        };
        cx.observe(&places, |_, places, cx| {
            update_jump_list(places.read(cx));
            cx.notify()
        })
        .detach();
        let address = cx.new(|cx| InputState::new(window, cx).placeholder(t!("Yol girin")));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("Ara")));
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("Dosya adı")));

        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(&address, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => {
                let text = match this.addr_sel.and_then(|i| this.addr_suggest.get(i)) {
                    Some(p) => p.to_string_lossy().into_owned(),
                    None => input.read(cx).value().to_string(),
                };
                this.clear_suggestions();
                this.editing_address = false;
                if text.trim().to_lowercase().starts_with("shell:") {
                    // resolve in the background (shell namespace), then navigate
                    let t = text.trim().to_string();
                    let work = cx.background_spawn(async move { rf_win::shell::resolve_shell_path(&t) });
                    cx.spawn_in(window, async move |this, cx| {
                        if let Some(p) = work.await {
                            let _ = this.update_in(cx, |ws, window, cx| ws.navigate(Location::Path(p), window, cx));
                        }
                    })
                    .detach();
                    this.focus_active(window, cx);
                    cx.notify();
                    return;
                }
                if let Some(loc) = parse_location(&text) {
                    this.navigate(loc, window, cx);
                }
                this.focus_active(window, cx);
                cx.notify();
            }
            InputEvent::Blur => {
                this.editing_address = false;
                this.clear_suggestions();
                cx.notify();
            }
            InputEvent::Change => {
                // the highlight belongs to the previous text
                this.addr_sel = None;
                if this.editing_address {
                    let text = input.read(cx).value().to_string();
                    this.update_suggestions(text, cx);
                }
            }
            _ => {}
        }));
        subs.push(cx.subscribe_in(&filter_input, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => {
                let q = input.read(cx).value().to_string();
                if let Some(tab) = this.tabs.get(this.active).map(|t| t.view().clone()) {
                    tab.update(cx, |v, cx| v.set_query(q, cx));
                }
            }
            InputEvent::PressEnter { .. } => this.focus_active(window, cx),
            _ => {}
        }));
        subs.push(cx.subscribe_in(&search, window, |this, input, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                // Enter: search this folder and all subfolders.
                let q = input.read(cx).value().trim().to_string();
                let root = this.active_tab().read(cx).location().path().map(Path::to_path_buf);
                if let (false, Some(root)) = (q.is_empty(), root) {
                    this.active_tab().clone().update(cx, |v, cx| v.navigate(Location::Search { root, query: q }, cx));
                    this.focus_active(window, cx);
                }
                return;
            }
            if let InputEvent::Change = ev {
                let q = input.read(cx).value().to_string();
                if let Some(tab) = this.tabs.get(this.active).map(|t| t.view().clone()) {
                    tab.update(cx, |v, cx| v.set_query(q, cx));
                }
            }
            // clicking elsewhere: the Omnibar shows the path again (the query stays as a chip)
            if let InputEvent::Blur = ev {
                if this.search_mode {
                    this.search_mode = false;
                    cx.notify();
                }
            }
        }));

        // Repaint the status-bar renderer badge when the hybrid renderer switches devices.
        cx.spawn(async move |this, cx| {
            let mut last = (gpui_windows::hybrid::switch_count(), gpui_windows::hybrid::mode());
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
                let now = (gpui_windows::hybrid::switch_count(), gpui_windows::hybrid::mode());
                if now != last {
                    last = now;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }
        })
        .detach();

        cx.observe_global::<crate::settings::Settings>(|this, cx| {
            // the language may have changed: the jump list follows
            update_jump_list(this.places.read(cx));
            let s = cx.global::<crate::settings::Settings>();
            // a file dialog keeps its own preview state (shown by default)
            if this.picker.is_none() {
                this.preview_open = s.preview_open;
                this.sidebar_rail = s.sidebar_rail;
            }
            this.sidebar_open = s.sidebar_open;
            cx.notify();
        })
        .detach();
        // the hosted preview (its own window) follows the app window when it moves
        cx.observe_window_bounds(window, |this: &mut Self, _, cx| {
            this.preview.update(cx, |pv, _| pv.invalidate_host());
            cx.notify();
        })
        .detach();
        let ws = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let wb = window.window_bounds();
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |w, cx| w.save_session(Some(wb), cx));
            }
            true
        });
        let prefs = cx.global::<crate::settings::Settings>().clone();
        {
            let entry = (window.window_handle(), cx.entity().downgrade());
            let list = &mut cx.default_global::<OpenWorkspaces>().0;
            list.retain(|(_, w)| w.upgrade().is_some());
            list.push(entry);
        }
        let shelf = cx.new(crate::shelf_view::ShelfView::new);
        let shelf_sub = cx.subscribe_in(&shelf, window, |this, _, ev, window, cx| this.on_shelf_event(ev, window, cx));

        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            closed: Vec::new(),
            places,
            images,
            ops,
            preview,
            quick_look: None,
            preview_open: prefs.preview_open,
            shelf,
            shelf_open: false,
            address,
            editing_address: false,
            search_mode: false,
            frame_dark: None,
            applied_backdrop: None,
            addr_suggest: Vec::new(),
            addr_sel: None,
            _addr_task: None,
            search,
            filter_bar: false,
            filter_input,
            sidebar_open: prefs.sidebar_open,
            compact: None,
            sidebar_w: prefs.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            sidebar_rail: prefs.sidebar_rail,
            rail_peek: [false; 2],
            rail_peek_bg: None,
            preview_w: prefs.preview_width.clamp(PREVIEW_MIN, PREVIEW_MAX),
            _width_save: None,
            palette: None,
            bulk: None,
            last_title: String::new(),
            tab_scroll: ScrollHandle::new(),
            tab_scrolled_to: None,
            tab_anim: None,
            tab_anim_serial: 0,
            drag_hover_tab: None,
            sidebar_ext_drag: false,
            picker: None,
            viewport_w: 1300.,
            last_session: Vec::new(),
            _bulk_sub: None,
            _palette_sub: None,
            _subs: { let mut s = subs; s.push(shelf_sub); s },
        };
        match start {
            Some(loc) => {
                let select = START_SELECT.lock().unwrap().take();
                this.open_tab_selecting(loc, select, window, cx)
            }
            None if prefs.startup == crate::settings::StartupPref::Continue && !prefs.session_tabs.is_empty() => {
                for t in &prefs.session_tabs {
                    let loc = if t.is_empty() { Location::Home } else { parse_launch(t).0 };
                    this.open_tab(loc, window, cx);
                }
                let a = prefs.session_active.min(this.tabs.len() - 1);
                this.activate(a, window, cx);
            }
            None if prefs.startup == crate::settings::StartupPref::Pages && !prefs.startup_pages.is_empty() => {
                for t in &prefs.startup_pages {
                    let loc = parse_location(t).unwrap_or(Location::Home);
                    this.open_tab(loc, window, cx);
                }
                this.activate(0, window, cx);
            }
            None => this.open_tab(Location::Home, window, cx),
        }
        this
    }

    /// Open/Save dialog window: its answer, filter and session (see `picker`).
    /// The preview pane on/off: the saved setting, or only this window when it is a file dialog.
    fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        let v = !self.preview_open;
        if self.picker.is_some() {
            self.preview_open = v;
            cx.notify();
        } else {
            crate::settings::update(cx, |s| s.preview_open = v);
        }
    }

    pub(crate) fn set_picker(&mut self, session: std::rc::Rc<crate::picker::PickSession>, window: &mut Window, cx: &mut Context<Self>) {
        let (patterns, folders_only) = (session.patterns(), session.folders_only());
        for t in &self.tabs {
            for v in std::iter::once(&t.left).chain(t.right.as_ref()) {
                v.update(cx, |v, cx| v.set_picking(patterns.clone(), folders_only, cx));
            }
        }
        self.picker = Some(session);
        // the preview pane helps picking (Windows' dialogs have one too): on by default here,
        // and the sidebar starts as a strip of icons (it opens over the list while hovered)
        self.preview_open = true;
        self.sidebar_rail = true;
        // closing the window (X, Alt+F4) cancels; it is not the user's session
        let ws = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            if let Some(ws) = ws.upgrade() {
                ws.update(cx, |ws, _| crate::picker::answer_cancel(ws));
            }
            true
        });
        cx.notify();
    }

    pub(crate) fn picker(&self) -> Option<std::rc::Rc<crate::picker::PickSession>> {
        self.picker.clone()
    }

    pub(crate) fn active_view(&self) -> Entity<FolderView> {
        self.active_tab().clone()
    }

    /// Open/Save dialog: another file type chosen (1-based).
    pub(crate) fn set_pick_filter(&mut self, ix: u32, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pk) = self.picker.clone() else { return };
        pk.filter_ix.set(ix);
        let patterns = pk.patterns();
        for t in &self.tabs {
            for v in std::iter::once(&t.left).chain(t.right.as_ref()) {
                v.update(cx, |v, cx| v.set_picking(patterns.clone(), pk.folders_only(), cx));
            }
        }
        cx.notify();
    }

    pub fn save_session(&mut self, bounds: Option<WindowBounds>, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        let placement = bounds.map(|wb| {
            let (b, max) = match wb {
                WindowBounds::Windowed(b) => (b, 0.),
                WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, 1.),
            };
            [f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height), max]
        });
        let tabs: Vec<String> = self
            .tabs
            .iter()
            .map(|t| session_text(t.left.read(cx).location()))
            .collect();
        let active = self.active;
        cx.update_global::<crate::settings::Settings, _>(|s, _| {
            s.session_tabs = tabs;
            s.session_active = active;
            if placement.is_some() {
                s.window = placement;
            }
        });
        // synchronous: the process is about to exit
        cx.global::<crate::settings::Settings>().save();
    }

    fn active_tab(&self) -> &Entity<FolderView> {
        self.tabs[self.active].view()
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self.active_tab().read(cx).focus.clone();
        window.focus(&fh, cx);
    }

    fn new_view(&mut self, loc: Location, window: &mut Window, cx: &mut Context<Self>) -> Entity<FolderView> {
        let places = self.places.clone();
        let images = self.images.clone();
        let tab = cx.new(|cx| FolderView::new(loc, places, images, cx));
        if let Some(pk) = self.picker.clone() {
            tab.update(cx, |v, cx| v.set_picking(pk.patterns(), pk.folders_only(), cx));
        }
        let sub = cx.subscribe_in(&tab, window, |this, emitter, ev: &FolderEvent, window, cx| match ev {
            FolderEvent::Picked(paths) => crate::picker::picked(this, paths.clone(), window, cx),
            FolderEvent::ToOtherPane(kind, paths) => {
                if let Some(other) = this.other_pane(emitter) {
                    let dest = match other.read(cx).location() {
                        Location::Path(p) if !rf_win::ftp::is_ftp(p) && rf_win::network::server_root(p).is_none() => Some(p.clone()),
                        _ => None,
                    };
                    if let Some(dest) = dest {
                        crate::status_center::transfer(cx, *kind, paths.clone(), dest);
                    }
                }
            }
            FolderEvent::OpenInOtherPane(loc) => {
                if let Some(other) = this.other_pane(emitter) {
                    other.update(cx, |v, cx| v.navigate(loc.clone(), cx));
                }
            }
            FolderEvent::OpenInNewTab(loc) => {
                let keep = this.active;
                this.open_tab(loc.clone(), window, cx);
                // a view asked for by name (disk usage) is what the user wants to see now
                let wanted = matches!(loc, Location::DiskUsage(_));
                if !wanted && !cx.global::<crate::settings::Settings>().focus_new_tab {
                    this.activate(keep, window, cx);
                }
            }
            FolderEvent::OpenInNewWindow(loc) => {
                let loc = loc.clone();
                let mut b = window.bounds();
                b.origin.x += px(36.);
                b.origin.y += px(36.);
                cx.defer(move |cx| crate::open_main_window(Some(loc), None, Some(WindowBounds::Windowed(b)), cx));
            }
            FolderEvent::QuickLook => this.toggle_quick_look(cx),
            FolderEvent::NeedPassword(archive, wrong) => {
                let view = emitter.downgrade();
                crate::archive_view::ask_password(archive.clone(), *wrong, window, cx, move |cx| {
                    let _ = view.update(cx, |v, cx| v.refresh(cx));
                });
            }
            FolderEvent::BulkRename(paths) => this.open_bulk_rename(paths.clone(), window, cx),
            FolderEvent::AddToShelf(paths) => {
                let paths = paths.clone();
                this.shelf.update(cx, |s, cx| {
                    s.add(paths, cx);
                });
                this.shelf_open = true;
                cx.notify();
            }
        });
        self._subs.push(sub);
        cx.observe(&tab, |_, _, cx| cx.notify()).detach();
        tab
    }

    /// `open_tab`, then selects `select` (a name in that folder) once it is listed.
    pub fn open_tab_selecting(&mut self, loc: Location, select: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tab(loc, window, cx);
        if let Some(name) = select {
            self.active_tab().update(cx, |v, _| v.set_reselect(name));
        }
    }

    pub fn open_tab(&mut self, loc: Location, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.new_view(loc, window, cx);
        self.tabs.push(Tab { left: view.clone(), right: None, right_active: false });
        let ix = self.tabs.len() - 1;
        // Settings → Appearance → Animations: the "+" grows into the new tab, then a new "+"
        // slides out from behind it. Purely visual: the tab is already open and usable.
        let animate = crate::settings::animations();
        cx.set_reduce_motion(!animate);
        if animate {
            self.tab_anim_serial += 1;
            let serial = self.tab_anim_serial;
            self.tab_anim = Some((view, serial));
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(TAB_ANIM_MS + PLUS_ANIM_MS + 100)).await;
                let _ = this.update(cx, |ws, cx| {
                    if ws.tab_anim.as_ref().is_some_and(|(_, s)| *s == serial) {
                        ws.tab_anim = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.activate(ix, window, cx);
        // Settings → "Open new tabs in dual pane"
        if cx.global::<crate::settings::Settings>().dual_pane_new_tabs && self.tabs[ix].left.read(cx).location().path().is_some() {
            self.toggle_dual_pane(window, cx);
        }
    }

    /// Files are being dragged; `over` tells whether the pointer is on tab `i`. Held on an
    /// inactive tab for a moment, that tab comes forward so the files can be dropped into it.
    fn drag_over_tab(&mut self, i: usize, over: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.drag_hover_tab.map(|(t, _)| t);
        if !over {
            if current == Some(i) {
                self.drag_hover_tab = None;
            }
            return;
        }
        if i == self.active || current == Some(i) {
            return;
        }
        self.tab_anim_serial += 1;
        let serial = self.tab_anim_serial;
        self.drag_hover_tab = Some((i, serial));
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(DRAG_HOVER_MS)).await;
            let _ = this.update_in(cx, |ws, window, cx| {
                // still on the same tab and still dragging
                if ws.drag_hover_tab == Some((i, serial)) && cx.has_active_drag() && i < ws.tabs.len() {
                    ws.drag_hover_tab = None;
                    ws.activate(i, window, cx);
                }
            });
        })
        .detach();
    }

    /// Opens/closes the second pane of the current tab (it starts at the same folder).
    fn toggle_dual_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        if self.tabs[ix].right.is_some() {
            self.tabs[ix].right = None;
            self.tabs[ix].right_active = false;
            self.tabs[ix].left.update(cx, |v, _| v.dual = false);
        } else {
            let loc = self.tabs[ix].left.read(cx).location().clone();
            let view = self.new_view(loc, window, cx);
            view.update(cx, |v, _| v.dual = true);
            self.tabs[ix].left.update(cx, |v, _| v.dual = true);
            self.tabs[ix].right = Some(view);
            self.tabs[ix].right_active = true;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// The pane next to `view` when its tab is split.
    fn other_pane(&self, view: &Entity<FolderView>) -> Option<Entity<FolderView>> {
        self.tabs.iter().find_map(|t| match &t.right {
            Some(r) if r == view => Some(t.left.clone()),
            Some(r) if &t.left == view => Some(r.clone()),
            _ => None,
        })
    }

    fn set_active_pane(&mut self, right: bool, cx: &mut Context<Self>) {
        let t = &mut self.tabs[self.active];
        if t.right.is_some() && t.right_active != right {
            t.right_active = right;
            cx.notify();
        }
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = ix;
        let q = self.active_tab().read(cx).filter.query.clone();
        self.search.update(cx, |s, cx| s.set_value(q, window, cx));
        self.editing_address = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        // the active index may stay the same while the tab under it changes: re-scroll
        self.tab_scrolled_to = None;
        if self.tabs.len() <= 1 {
            window.remove_window();
            return;
        }
        let tab = self.tabs.remove(ix);
        self.closed.push(tab.view().read(cx).location().clone());
        let next = if self.active > ix || self.active >= self.tabs.len() {
            self.active.saturating_sub(1)
        } else {
            self.active
        };
        self.activate(next.min(self.tabs.len() - 1), window, cx);
    }

    fn close_other_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keep = self.active;
        for i in (0..self.tabs.len()).rev() {
            if i != keep {
                let tab = self.tabs.remove(i);
                self.closed.push(tab.view().read(cx).location().clone());
            }
        }
        self.tab_scrolled_to = None;
        self.activate(0, window, cx);
    }

    fn close_tabs_to_side(&mut self, ix: usize, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        let range: Vec<usize> = if right { (ix + 1..self.tabs.len()).collect() } else { (0..ix).collect() };
        for i in range.into_iter().rev() {
            let tab = self.tabs.remove(i);
            self.closed.push(tab.view().read(cx).location().clone());
        }
        let ix = if right { ix } else { 0 };
        self.tab_scrolled_to = None;
        self.activate(ix.min(self.tabs.len() - 1), window, cx);
    }

    fn move_tab(&mut self, from: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let active_left = self.tabs[self.active].left.clone();
        let t = self.tabs.remove(from);
        self.tabs.insert(to, t);
        self.active = self.tabs.iter().position(|t| t.left == active_left).unwrap_or(0);
        let _ = window;
        cx.notify();
    }

    fn tab_menu(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        use crate::fluent_menu::{Item, Menu, MenuIcon};
        let me = cx.entity().downgrade();
        let act = |f: fn(&mut Workspace, usize, &mut Window, &mut Context<Workspace>)| {
            let me = me.clone();
            move |w: &mut Window, cx: &mut App| {
                let _ = me.update(cx, |ws, cx| f(ws, ix, w, cx));
            }
        };
        let n = self.tabs.len();
        let items = vec![
            Item::action(t!("Yeni sekme"), MenuIcon::Lucide(L::Plus), act(|ws, _, w, cx| ws.open_tab(Location::Home, w, cx))).hint("Ctrl+T"),
            Item::action(t!("Sekmeyi çoğalt"), MenuIcon::Lucide(L::Copy), act(|ws, ix, w, cx| {
                if let Some(loc) = ws.tabs.get(ix).map(|t| t.view().read(cx).location().clone()) {
                    ws.open_tab(loc, w, cx);
                }
            }))
            .hint("Ctrl+Shift+K"),
            Item::action(t!("Yeni pencereye taşı"), MenuIcon::Lucide(L::AppWindow), act(|ws, ix, w, cx| ws.detach_tab(ix, w, cx))).enabled(n > 1),
            Item::Separator,
            Item::action(t!("Diğer sekmeleri kapat"), MenuIcon::None, act(|ws, ix, w, cx| {
                ws.active = ix;
                ws.close_other_tabs(w, cx)
            }))
            .enabled(n > 1),
            Item::action(t!("Soldaki sekmeleri kapat"), MenuIcon::None, act(|ws, ix, w, cx| ws.close_tabs_to_side(ix, false, w, cx))).enabled(ix > 0),
            Item::action(t!("Sağdaki sekmeleri kapat"), MenuIcon::None, act(|ws, ix, w, cx| ws.close_tabs_to_side(ix, true, w, cx))).enabled(ix + 1 < n),
            Item::action(t!("Sekmeyi kapat"), MenuIcon::Lucide(L::X), act(|ws, ix, w, cx| ws.close_tab(ix, w, cx))).hint("Ctrl+W"),
            Item::Separator,
            Item::action(t!("Kapatılan sekmeyi yeniden aç"), MenuIcon::Lucide(L::RotateCcw), act(|ws, _, w, cx| {
                if let Some(loc) = ws.closed.pop() {
                    ws.open_tab(loc, w, cx);
                }
            }))
            .hint("Ctrl+Shift+T")
            .enabled(!self.closed.is_empty()),
        ];
        crate::fluent_menu::show(Menu { quick: Vec::new(), items }, window.mouse_position(), window, cx);
    }

    /// Breadcrumb chevron: pick a subfolder of `dir` (or a drive when `dir` is None).
    fn crumb_menu(&mut self, dir: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let is_drives = dir.is_none();
        // List on a worker (can stall on network/optical drives); the shell thread only shows the menu.
        let listing = cx.background_spawn(async move {
            let mut entries: Vec<(String, PathBuf)> = match &dir {
                None => rf_win::list_drives().into_iter().map(|d| (crate::places::drive_name(&d), d.root)).collect(),
                Some(d) => {
                    let mut v = Vec::new();
                    let _ = rf_win::list_dir(d, 4096, &|| false, &mut |b| {
                        for e in b {
                            if e.is_dir() && !e.is_hidden() {
                                v.push((e.name.to_string(), d.join(&*e.name)));
                            }
                        }
                    });
                    v.sort_by(|a, b| rf_core::sort::natural_cmp(&a.0, &b.0));
                    v
                }
            };
            entries.truncate(80);
            entries
        });
        let pos = window.mouse_position() + point(px(-12.), px(14.));
        cx.spawn_in(window, async move |this, cx| {
            use crate::fluent_menu::{Item, Menu, MenuIcon};
            let entries = listing.await;
            let _ = this.update_in(cx, |_, window, cx| {
                let me = cx.entity().downgrade();
                let folder = MenuIcon::Tinted(L::Folder, crate::theme::kind_color(crate::icons::Category::Folder, true));
                let items: Vec<Item> = if entries.is_empty() {
                    vec![Item::action(t!("(alt klasör yok)"), MenuIcon::None, |_, _| {}).enabled(false)]
                } else {
                    entries
                        .into_iter()
                        .map(|(n, path)| {
                            let me = me.clone();
                            let icon = if is_drives { MenuIcon::Lucide(L::HardDrive) } else { folder.clone() };
                            Item::action(n, icon, move |w, cx| {
                                let p = path.clone();
                                let _ = me.update(cx, |ws, cx| ws.navigate(Location::Path(p), w, cx));
                            })
                        })
                        .collect()
                };
                crate::fluent_menu::show(Menu { quick: Vec::new(), items }, pos, window, cx);
            });
        })
        .detach();
    }

    /// Boşluk: DirectLook'a (ayrı süreç) aç/kapat iletisi. DirectLook kapalıyken bir şey yapmaz.
    fn toggle_quick_look(&mut self, cx: &mut Context<Self>) {
        use std::sync::atomic::Ordering;
        if !cx.global::<crate::settings::Settings>().directlook_enabled {
            return;
        }
        let open = crate::folder_view::QUICK_LOOK_OPEN.load(Ordering::Relaxed);
        let target = self.active_tab().read(cx).preview_target().map(|(p, e)| (p, e.size));
        match (open, target) {
            (true, _) => {
                crate::look::ipc::send(crate::look::ipc::Msg::Close, false);
                crate::folder_view::QUICK_LOOK_OPEN.store(false, Ordering::Relaxed);
                *LOOK_FOLLOWED.lock().unwrap() = None;
            }
            (false, Some((path, size))) => {
                *LOOK_FOLLOWED.lock().unwrap() = Some(path.clone());
                // an item inside an archive is extracted first
                crate::archive_view::look_send(path, size, false);
                crate::folder_view::QUICK_LOOK_OPEN.store(true, Ordering::Relaxed);
            }
            (false, None) => {}
        }
        cx.notify();
    }

    fn release_drive(&mut self, letter: char, cx: &mut Context<Self>) {
        let views: Vec<Entity<FolderView>> =
            self.tabs.iter().flat_map(|t| std::iter::once(t.left.clone()).chain(t.right.clone())).collect();
        let mut moved = false;
        for v in views {
            let here = v.read(cx).location().path().is_some_and(|p| on_drive(p, letter));
            if here {
                v.update(cx, |v, cx| v.navigate(Location::Home, cx));
                moved = true;
            }
        }
        self.close_quick_look(cx);
        self.preview.update(cx, |pv, cx| pv.release_drive(letter, cx));
        self.images.update(cx, |c, _| c.cancel_pending());
        if moved {
            log::info!("{letter}: is being removed; its panes went Home");
        }
        cx.notify();
    }

    fn close_quick_look(&mut self, cx: &mut Context<Self>) {
        if let Some(ql) = self.quick_look.take() {
            // release the hosted preview window before the pane takes it back
            ql.update(cx, |pv, _| pv.set_occluded(true));
        }
        crate::folder_view::QUICK_LOOK_OPEN.store(false, std::sync::atomic::Ordering::Relaxed);
        cx.notify();
    }

    fn open_bulk_rename(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::bulk_rename::{BulkRename, BulkRenameEvent};
        if paths.len() < 2 {
            return;
        }
        let br = cx.new(|cx| BulkRename::new(paths, window, cx));
        self._bulk_sub = Some(cx.subscribe_in(&br, window, |this, _, ev: &BulkRenameEvent, window, cx| match ev {
            BulkRenameEvent::Close => {
                this.bulk = None;
                this._bulk_sub = None;
                this.with_tab(cx, |v, cx| v.soft_reload(cx));
                this.focus_active(window, cx);
                cx.notify();
            }
        }));
        self.bulk = Some(br);
        cx.notify();
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::command_palette::{CommandPalette, PaletteEvent};
        if self.palette.take().is_some() {
            self._palette_sub = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        let pal = cx.new(|cx| CommandPalette::new(window, cx));
        self._palette_sub = Some(cx.subscribe_in(&pal, window, |this, _, ev: &PaletteEvent, window, cx| {
            this.palette = None;
            this._palette_sub = None;
            this.focus_active(window, cx);
            match ev {
                PaletteEvent::Dismiss => {}
                PaletteEvent::Run(action) => window.dispatch_action(action.boxed_clone(), cx),
                PaletteEvent::Navigate(p) => {
                    if let Some(loc) = parse_location(&p.to_string_lossy()) {
                        this.navigate(loc, window, cx);
                    }
                }
            }
            cx.notify();
        }));
        self.palette = Some(pal);
        cx.notify();
    }

    /// The "Filter by" strip: typing filters this folder by name; closing clears the filter.
    fn toggle_filter_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_bar = !self.filter_bar;
        if self.filter_bar {
            self.filter_input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.filter_input.update(cx, |s, cx| s.set_value("", window, cx));
            self.with_tab(cx, |v, cx| v.set_query(String::new(), cx));
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn render_filter_bar(&mut self, p: &Palette, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.filter_bar {
            return None;
        }
        // follow the folder: entering another folder clears its filter
        let q = self.active_tab().read(cx).filter.query.clone();
        if self.filter_input.read(cx).value().as_ref() != q.as_str() {
            self.filter_input.update(cx, |s, cx| s.set_value(q, window, cx));
        }
        Some(
            h_flex()
                .px_4()
                .py_2()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(p.divider)
                .child(Icon::new(L::Folder).size(px(16.)).text_color(crate::theme::kind_color(crate::icons::Category::Folder, true)))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("Şunlara göre filtrele")))
                .child(
                    div()
                        .w(px(200.))
                        .h(px(30.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(theme::RADIUS_SM)
                        .bg(p.hover)
                        .border_1()
                        .border_color(p.border)
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" {
                                this.toggle_filter_bar(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(Input::new(&self.filter_input).appearance(false).cleanable(true)),
                )
                .into_any_element(),
        )
    }

    fn navigate(&mut self, loc: Location, window: &mut Window, cx: &mut Context<Self>) {
        self.active_tab().clone().update(cx, |v, cx| v.navigate(loc, cx));
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.focus_active(window, cx);
    }

    fn toggle_shelf(&mut self, cx: &mut Context<Self>) {
        self.shelf_open = !self.shelf_open;
        if self.shelf_open {
            self.shelf.update(cx, |s, cx| s.prune(cx));
        }
        cx.notify();
    }

    fn on_shelf_event(&mut self, ev: &crate::shelf_view::ShelfEvent, window: &mut Window, cx: &mut Context<Self>) {
        use crate::shelf_view::ShelfEvent;
        match ev {
            ShelfEvent::Transfer(kind, items) => {
                let dest = match self.active_tab().read(cx).location() {
                    Location::Path(p)
                        if !rf_win::ftp::is_ftp(p) && rf_win::network::server_root(p).is_none() =>
                    {
                        Some(p.clone())
                    }
                    _ => None,
                };
                if let Some(dest) = dest {
                    crate::status_center::transfer(cx, *kind, items.clone(), dest);
                    if *kind == rf_win::fileops::Transfer::Move {
                        // moved items leave their old place; the shelf is done with them
                        self.shelf.update(cx, |s, cx| s.clear(cx));
                    }
                }
            }
            ShelfEvent::Reveal(p) => {
                if let Some(parent) = p.parent() {
                    self.navigate(Location::Path(parent.to_path_buf()), window, cx);
                }
            }
            ShelfEvent::Close => {
                self.shelf_open = false;
                cx.notify();
            }
        }
    }

    /// The corner button's menu, like Files' "Tab actions": new window, compact overlay,
    /// split / arrange / close panes — plus the sidebar toggle that used to live there.
    fn tab_actions_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::fluent_menu::{Item, Menu, MenuIcon};
        let me = cx.entity().downgrade();
        let with = |f: fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
            let me = me.clone();
            move |w: &mut Window, cx: &mut App| {
                let _ = me.update(cx, |ws, cx| f(ws, w, cx));
            }
        };
        let split = self.tabs[self.active].right.is_some();
        let vertical = cx.global::<crate::settings::Settings>().dual_pane_vertical;
        let mut items = vec![
            Item::action(t!("Yeni pencere"), MenuIcon::Lucide(L::AppWindow), |w, cx| w.dispatch_action(Box::new(NewWindow), cx)).hint("Ctrl+N"),
            if self.compact.is_some() {
                Item::action(t!("Kompakt görünümden çık"), MenuIcon::Lucide(L::Maximize2), with(|ws, w, cx| ws.toggle_compact(w, cx)))
            } else {
                Item::action(t!("Kompakt görünüme geç"), MenuIcon::Lucide(L::PictureInPicture2), with(|ws, w, cx| ws.toggle_compact(w, cx)))
            },
        ];
        if split {
            items.push(Item::sub(
                t!("Bölmeleri düzenle"),
                MenuIcon::Lucide(L::LayoutPanelLeft),
                vec![
                    Item::action(t!("Yan yana"), MenuIcon::Lucide(L::Columns2), with(|ws, w, cx| ws.arrange_panes(true, w, cx))).checked(vertical),
                    Item::action(t!("Üst üste"), MenuIcon::Lucide(L::Rows2), with(|ws, w, cx| ws.arrange_panes(false, w, cx))).checked(!vertical),
                ],
            ));
            items.push(Item::action(t!("Etkin bölmeyi kapat"), MenuIcon::Lucide(L::X), with(|ws, w, cx| ws.close_active_pane(w, cx))));
        } else {
            items.push(Item::sub(
                t!("Bölmeyi ayır"),
                MenuIcon::Lucide(L::LayoutPanelLeft),
                vec![
                    Item::action(t!("Yan yana"), MenuIcon::Lucide(L::Columns2), with(|ws, w, cx| ws.split_pane(true, w, cx))).hint("Ctrl+Shift+D"),
                    Item::action(t!("Üst üste"), MenuIcon::Lucide(L::Rows2), with(|ws, w, cx| ws.split_pane(false, w, cx))),
                ],
            ));
        }
        items.push(Item::Separator);
        items.push(
            Item::action(t!("Kenar çubuğu"), MenuIcon::Lucide(L::PanelLeft), |w, cx| w.dispatch_action(Box::new(ToggleSidebar), cx))
                .checked(self.sidebar_open)
                .hint("Ctrl+B"),
        );
        let pos = point(px(6.), theme::TITLEBAR_H);
        crate::fluent_menu::show(Menu { quick: Vec::new(), items }, pos, window, cx);
    }

    fn split_pane(&mut self, side_by_side: bool, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::update(cx, |s| s.dual_pane_vertical = side_by_side);
        if self.tabs[self.active].right.is_none() {
            self.toggle_dual_pane(window, cx);
        }
    }

    fn arrange_panes(&mut self, side_by_side: bool, _: &mut Window, cx: &mut Context<Self>) {
        crate::settings::update(cx, |s| s.dual_pane_vertical = side_by_side);
        cx.notify();
    }

    /// Closes the focused pane of a split tab; the other one stays.
    fn close_active_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = &mut self.tabs[self.active];
        let Some(right) = t.right.take() else { return };
        if !t.right_active {
            t.left = right;
        }
        t.right_active = false;
        t.left.update(cx, |v, _| v.dual = false);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn hwnd(window: &Window) -> Option<isize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        }
    }

    /// Files' "compact overlay": a small always-on-top window with just the file list (sidebar
    /// and preview hidden); leaving restores the previous size, position and z-order.
    fn toggle_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hwnd) = Self::hwnd(window) else { return };
        if let Some(r) = self.compact.take() {
            rf_win::window::set_min_size(hwnd, None);
            rf_win::window::place(hwnd, r.rect);
            if r.maximized {
                rf_win::window::maximize(hwnd);
            }
            if !r.topmost {
                let _ = rf_win::window::set_topmost(hwnd, false);
            }
        } else {
            let Some((rect, maximized)) = rf_win::window::rect(hwnd) else { return };
            // restore to the normal (not maximized) rectangle later
            let normal = if maximized { None } else { Some(rect) };
            let topmost = rf_win::window::is_topmost(hwnd);
            let scale = window.scale_factor();
            let (w, h) = ((COMPACT_W * scale) as i32, (COMPACT_H * scale) as i32);
            rf_win::window::set_min_size(hwnd, Some((COMPACT_MIN.0, COMPACT_MIN.1)));
            // keep the top-right corner where it was: the overlay hugs the side it came from
            let (right, top) = (rect[2], rect[1].max(0));
            let restore = normal.unwrap_or_else(|| {
                let b = window.bounds();
                let o = |v: Pixels| (f32::from(v) * scale) as i32;
                [o(b.origin.x), o(b.origin.y), o(b.origin.x + b.size.width), o(b.origin.y + b.size.height)]
            });
            rf_win::window::place(hwnd, [right - w, top, right, top + h]);
            let _ = rf_win::window::set_topmost(hwnd, true);
            self.compact = Some(CompactRestore { rect: restore, maximized, topmost });
        }
        cx.notify();
    }

    fn toggle_always_on_top(&mut self, window: &mut Window) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                let hwnd = w.hwnd.get();
                let on = !rf_win::window::is_topmost(hwnd);
                if let Err(e) = rf_win::window::set_topmost(hwnd, on) {
                    log::warn!("always on top: {e}");
                }
            }
        }
    }

    fn with_tab(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut FolderView, &mut Context<FolderView>)) {
        self.active_tab().clone().update(cx, f);
    }

    fn clear_suggestions(&mut self) {
        self.addr_suggest.clear();
        self.addr_sel = None;
        self._addr_task = None;
    }

    /// Folder completions for the address text, computed off the UI thread (network paths can
    /// be slow); stale answers are dropped with the task.
    fn update_suggestions(&mut self, text: String, cx: &mut Context<Self>) {
        // "@pro": the matching folder aliases
        if text.trim_start().starts_with(crate::aliases::PREFIX) && !text.contains(['\\', '/']) {
            self._addr_task = None;
            self.addr_suggest = crate::aliases::matches(&text).into_iter().map(|(_, p)| p).take(8).collect();
            self.addr_sel = None;
            cx.notify();
            return;
        }
        let expanded = expand_env(text.trim());
        if !rf_win::complete::completable(&expanded) {
            self.clear_suggestions();
            cx.notify();
            return;
        }
        let hidden = cx.global::<crate::settings::Settings>().show_hidden;
        let work = cx.background_spawn(async move { rf_win::complete::complete_dirs(&expanded, 8, hidden) });
        self._addr_task = Some(cx.spawn(async move |this, cx| {
            let list = work.await;
            let _ = this.update(cx, |ws, cx| {
                if ws.editing_address {
                    ws.addr_suggest = list;
                    ws.addr_sel = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Tab (arrives as the input's unhandled `IndentInline`): takes the highlighted (or first)
    /// completion into the text and lists its children.
    fn accept_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_address || self.addr_suggest.is_empty() {
            cx.propagate();
            return;
        }
        let pick = self.addr_sel.unwrap_or(0);
        if let Some(p) = self.addr_suggest.get(pick).cloned() {
            let text = format!("{}\\", p.to_string_lossy().trim_end_matches('\\'));
            self.address.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
            self.update_suggestions(text, cx);
        }
    }

    /// Up/Down move the highlight; Escape closes the list.
    fn address_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_address || self.addr_suggest.is_empty() {
            return;
        }
        let n = self.addr_suggest.len();
        match ev.keystroke.key.as_str() {
            "down" => {
                self.addr_sel = Some(self.addr_sel.map_or(0, |i| (i + 1) % n));
                cx.stop_propagation();
                cx.notify();
            }
            "up" => {
                self.addr_sel = Some(self.addr_sel.map_or(n - 1, |i| (i + n - 1) % n));
                cx.stop_propagation();
                cx.notify();
            }
            "escape" => {
                self.clear_suggestions();
                cx.notify();
            }
            _ => {}
        }
    }

    fn render_suggestions(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.editing_address || self.addr_suggest.is_empty() {
            return None;
        }
        let alias_mode = self.address.read(cx).value().trim_start().starts_with(crate::aliases::PREFIX);
        let mut list = v_flex()
            .absolute()
            .top(px(38.))
            .left_0()
            .right_0()
            .py_1()
            .rounded(theme::RADIUS)
            .bg(p.card_bg)
            .border_1()
            .border_color(p.border)
            .shadow_lg();
        for (i, path) in self.addr_suggest.iter().enumerate() {
            let sel = self.addr_sel == Some(i);
            let target = path.clone();
            list = list.child(
                h_flex()
                    .id(("addr-sug", i))
                    .mx_1()
                    .px_2()
                    .h(px(28.))
                    .gap_2()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .when(sel, |d| d.bg(p.selection))
                    .hover(|s| s.bg(p.hover))
                    .cursor_pointer()
                    .child(Icon::new(L::Folder).size(px(14.)).text_color(p.accent))
                    .children(alias_mode.then(|| crate::aliases::name_of(path)).flatten().map(|n| {
                        div().flex_shrink_0().font_weight(FontWeight::SEMIBOLD).child(format!("@{n}"))
                    }))
                    .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().when(alias_mode, |d| d.text_color(p.text_secondary)).child(path.to_string_lossy().into_owned()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.clear_suggestions();
                            this.editing_address = false;
                            this.navigate(Location::Path(target.clone()), window, cx);
                            this.focus_active(window, cx);
                            cx.notify();
                        }),
                    ),
            );
        }
        Some(deferred(list).with_priority(2).into_any_element())
    }

    fn begin_address_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match self.active_tab().read(cx).location() {
            Location::Path(p) => p.to_string_lossy().into_owned(),
            Location::Search { root, .. } => root.to_string_lossy().into_owned(),
            _ => String::new(),
        };
        self.editing_address = true;
        self.address.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        cx.notify();
    }

    // ---------- rendering ----------

    /// A menu, flyout or the Status Center is open: buttons under it show no tooltips (one
    /// would be painted over the popup).
    fn popups_open(&self, cx: &App) -> bool {
        crate::fluent_menu::is_open(cx) || crate::flyout::is_open(cx) || self.ops.read(cx).open
    }

    fn render_titlebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let popups_open = self.popups_open(cx);
        let mut tabs = h_flex()
            .id("tabs")
            // content-sized, shrinks (and scrolls) only when there is not enough room, so the
            // "+" stays right after the last tab
            .flex_shrink(1.)
            .min_w_0()
            .h_full()
            .items_end()
            .gap_1()
            .pl_1()
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll);
        if self.tab_scrolled_to != Some(self.active) {
            // only when the active tab changes, so manual scrolling is not fought every frame
            self.tab_scrolled_to = Some(self.active);
            self.tab_scroll.scroll_to_item(self.active);
        }
        // Equal-width tabs that share the strip (like Files / WinUI TabView "Equal"): wide
        // while there are few, shrinking as more open, scrolling only below the minimum.
        let strip = (self.viewport_w - 150. - TITLEBAR_LEAD - 40.).max(120.);
        let n = self.tabs.len().max(1) as f32;
        let tab_w = ((strip - 4.) / n - 4.).clamp(TAB_MIN_W, TAB_MAX_W);
        let roomy = tab_w >= 110.;
        for (i, tab) in self.tabs.iter().enumerate() {
            let v = tab.view().read(cx);
            let title = v.title(cx);
            let icon = v.tab_icon();
            let tab_dir = v.location().path().map(Path::to_path_buf);
            let active = i == self.active;
            let drag = TabDrag { ix: i, title: title.clone().into() };
            let accent = p.accent;
            let drop_bg = p.selection;
            let anim = self.tab_anim.as_ref().filter(|(v, _)| *v == tab.left).map(|(_, s)| *s);
            let el = h_flex()
                    .id(("tab", i))
                    .group("tab")
                    .flex_shrink_0()
                    .h(px(32.))
                    .w(px(tab_w))
                    .pl_3()
                    .pr_1()
                    .gap_2()
                    .items_center()
                    .rounded_t(theme::RADIUS)
                    .text_sm()
                    .when(active, |d| d.bg(p.tab_active))
                    .when(!active, |d| d.text_color(p.text_secondary).hover(|s| s.bg(p.hover)))
                    .child(Icon::new(icon).size(px(14.)).flex_shrink_0().text_color(if active { p.accent } else { p.text_secondary }))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                    .child(
                        div()
                            .id(("tab-close", i))
                            .size(px(22.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_SM)
                            // like Files: every tab shows its close button while there is room
                            .when(!roomy && !active, |d| d.invisible())
                            .group_hover("tab", |s| s.visible())
                            .hover(|s| s.bg(p.pressed))
                            .child(Icon::new(L::X).size(px(12.)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(i, window, cx);
                            })),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            // Keep the title bar from turning a tab drag into a window move.
                            cx.stop_propagation();
                            this.activate(i, window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| this.close_tab(i, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| this.tab_menu(i, window, cx)),
                    )
                    // files dragged over a tab and held there switch to it (Explorer / Files)
                    .on_drag_move::<ExternalPaths>(cx.listener(move |this, ev: &DragMoveEvent<ExternalPaths>, window, cx| {
                        let over = ev.bounds.contains(&ev.event.position);
                        this.drag_over_tab(i, over, window, cx);
                    }))
                    .on_drag(drag, |d: &TabDrag, _, _, cx| cx.new(|_| d.clone()))
                    .drag_over::<TabDrag>(move |s, _, _, _| s.border_l_2().border_color(accent))
                    .on_drop(cx.listener(move |this, d: &TabDrag, window, cx| this.move_tab(d.ix, i, window, cx)))
                    .when_some(tab_dir, |d, dir| {
                        d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
                            .on_drop(move |paths: &ExternalPaths, _, cx| drop_files(paths, dir.clone(), cx))
                    });
            tabs = match anim {
                // grows from the "+" box (28 px) to its width and height, contents fading in
                Some(serial) => tabs.child(el.overflow_hidden().with_animation(
                    ("tab-open", serial),
                    Animation::new(std::time::Duration::from_millis(TAB_ANIM_MS)).with_easing(ease_out_quint()),
                    move |el, d| el.w(px(28. + (tab_w - 28.) * d)).h(px(28. + 4. * d)).opacity(0.25 + 0.75 * d),
                )),
                None => tabs.child(el),
            };
        }
        // outside the scrolling strip: always reachable
        let new_tab = div()
                .id("new-tab")
                .flex_shrink_0()
                .ml_1()
                .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Yeni sekme  (Ctrl+T)")).build(w, cx))
                .size(px(28.))
                .mb(px(2.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .hover(|s| s.bg(p.hover))
                .child(Icon::new(L::Plus).size(px(14.)))
                // the title bar arms a window move on mouse down: the slightest jitter then
                // swallows the mouse up and the click never happens
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| this.open_tab(Location::Home, window, cx)));
        // after the new tab has grown, the next "+" slides out from behind it
        let new_tab: AnyElement = match self.tab_anim.as_ref().map(|(_, s)| *s) {
            Some(serial) => {
                let total = (TAB_ANIM_MS + PLUS_ANIM_MS) as f32;
                let start = TAB_ANIM_MS as f32 / total;
                new_tab
                    .relative()
                    .with_animation(("plus-in", serial), Animation::new(std::time::Duration::from_millis(TAB_ANIM_MS + PLUS_ANIM_MS)), move |el, d| {
                        let t = ((d - start) / (1. - start)).clamp(0., 1.);
                        let e = ease_out_quint()(t);
                        el.left(px(-26. * (1. - e))).opacity(e)
                    })
                    .into_any_element()
            }
            None => new_tab.into_any_element(),
        };

        TitleBar::new()
            .h(theme::TITLEBAR_H)
            .bg(p.titlebar_bg)
            .border_color(gpui_kit::transparent_black())
            .child(
                h_flex()
                    .h_full()
                    // explicit width: the component's title bar never shrinks its content, so
                    // many tabs would push the window buttons off-screen (min/max/close ~140 px)
                    .w(px((self.viewport_w - 150.).max(200.)))
                    .min_w_0()
                    .overflow_hidden()
                    .items_end()
                    .child(
                        // sidebar toggle in the corner (Files has the pane button here, no app name)
                        // same size and baseline as the "+" button; centered over the Back
                        // button below (title bar pads 12 px, the nav row 8 px)
                        h_flex().flex_shrink_0().w(px(TITLEBAR_LEAD)).h_full().items_end().child(
                            div()
                                .id("tab-actions")
                                .ml(px(-2.))
                                .mb(px(2.))
                                .size(px(28.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::RADIUS_SM)
                                .text_color(p.text_secondary)
                                .hover(|s| s.bg(p.hover))
                                .when(!popups_open, |d| d.tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Sekme eylemleri")).build(w, cx)))
                                .child(Icon::new(if self.compact.is_some() { L::PictureInPicture2 } else { L::PanelLeft }).size(px(16.)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, window, cx| this.tab_actions_menu(window, cx))),
                        ),
                    )
                    .child(tabs)
                    .child(new_tab),
            )
    }

    /// Live pane resize; the width is kept in settings (written with the next session save).
    fn set_pane_width(&mut self, sidebar: bool, w: f32, cx: &mut Context<Self>) {
        if sidebar {
            let rail = if self.sidebar_rail { w < RAIL_SNAP + 30. } else { w < RAIL_SNAP };
            if rail != self.sidebar_rail {
                self.sidebar_rail = rail;
                if self.picker.is_none() {
                    cx.update_global::<crate::settings::Settings, _>(|s, _| s.sidebar_rail = rail);
                }
                cx.notify();
            }
            if rail {
                return;
            }
        }
        let w = if sidebar { w.clamp(SIDEBAR_MIN, SIDEBAR_MAX) } else { w.clamp(PREVIEW_MIN, PREVIEW_MAX) };
        let cur = if sidebar { &mut self.sidebar_w } else { &mut self.preview_w };
        if (*cur - w).abs() < 0.5 {
            return;
        }
        *cur = w;
        cx.update_global::<crate::settings::Settings, _>(|s, _| {
            if sidebar {
                s.sidebar_width = w;
            } else {
                s.preview_width = w;
            }
        });
        // write once the drag has settled
        self._width_save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
            let _ = cx.update(|cx| {
                let snapshot = cx.global::<crate::settings::Settings>().clone();
                std::thread::spawn(move || snapshot.save());
            });
        }));
        cx.notify();
    }

    /// The pin list as it is right now: the user's list from settings (updated synchronously),
    /// else the defaults shown by `Places`. `Places` itself refreshes asynchronously, so reading it
    /// right after a change would undo that change.
    fn current_pins(&self, cx: &App) -> Vec<PathBuf> {
        cx.global::<crate::settings::Settings>()
            .pinned
            .clone()
            .unwrap_or_else(|| self.places.read(cx).pinned.iter().map(|f| f.path.clone()).collect())
    }

    /// Reorders pinned entries (drag & drop in the sidebar); persisted as the user's pin list.
    /// By path, not index: the drawn list may still show the order from before a previous drag.
    fn move_pin(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        let mut list = self.current_pins(cx);
        let (Some(from), Some(to)) = (list.iter().position(|p| p == from), list.iter().position(|p| p == to)) else {
            return;
        };
        if from == to {
            return;
        }
        let item = list.remove(from);
        list.insert(to, item);
        crate::settings::update(cx, |s| s.pinned = Some(list));
        self.places.update(cx, |p, cx| p.refresh(cx));
    }

    /// Right-click on a sidebar folder/drive: the Explorer menu (eject, properties, format…)
    /// plus open in new tab/window and pin/unpin.
    fn sidebar_menu(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        use crate::fluent_menu::{Item, Menu, MenuIcon, Sub};
        let shift = unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT.0 as i32) < 0
        };
        if shift {
            // the classic Explorer menu, extended verbs included
            let p2 = path.clone();
            rf_win::sta::run(move |owner| {
                let mut pt = windows::Win32::Foundation::POINT::default();
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
                }
                if let Err(e) = rf_win::ctxmenu::show_for_one(owner, &p2, (pt.x, pt.y), true, &[]) {
                    log::warn!("sidebar menu: {e:#}");
                }
            });
            return;
        }
        let me = cx.entity().downgrade();
        let is_pinned = self.current_pins(cx).contains(&path);
        let with = |f: fn(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>)| {
            let (me, path) = (me.clone(), path.clone());
            move |w: &mut Window, cx: &mut App| {
                let p = path.clone();
                let _ = me.update(cx, |ws, cx| f(ws, p, w, cx));
            }
        };
        let items = vec![
            Item::action(t!("Aç"), MenuIcon::Lucide(L::SquareArrowOutUpRight), with(|ws, p, w, cx| ws.navigate(Location::Path(p), w, cx))),
            Item::action(t!("Yeni sekmede aç"), MenuIcon::Lucide(L::SquarePlus), with(|ws, p, w, cx| {
                let keep = ws.active;
                ws.open_tab(Location::Path(p), w, cx);
                if !cx.global::<crate::settings::Settings>().focus_new_tab {
                    ws.activate(keep, w, cx);
                }
            })),
            Item::action(t!("Yeni pencerede aç"), MenuIcon::Lucide(L::AppWindow), with(|_, p, w, cx| {
                let mut b = w.bounds();
                b.origin.x += px(36.);
                b.origin.y += px(36.);
                cx.defer(move |cx| crate::open_main_window(Some(Location::Path(p)), None, Some(WindowBounds::Windowed(b)), cx));
            })),
            Item::Separator,
            if is_pinned {
                Item::action(t!("Kenar çubuğundan kaldır"), MenuIcon::Lucide(L::PinOff), with(|ws, p, _, cx| ws.set_pinned(p, false, cx)))
            } else {
                Item::action(t!("Kenar çubuğuna sabitle"), MenuIcon::Lucide(L::Pin), with(|ws, p, _, cx| ws.set_pinned(p, true, cx)))
            },
            Item::action(t!("Yolu kopyala"), MenuIcon::Lucide(L::ClipboardCopy), with(|_, p, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(p.display().to_string()))
            })),
            Item::action(t!("Terminalde aç"), MenuIcon::Lucide(L::SquareTerminal), with(|_, p, _, _| {
                std::thread::spawn(move || {
                    let _ = rf_win::shell::open_terminal(&p);
                });
            })),
            // our Properties window (a drive gets its own page there)
            Item::action(t!("Özellikler"), MenuIcon::Lucide(L::Wrench), with(|ws, p, _, cx| {
                crate::properties_view::open(vec![p], ws.images.clone(), cx);
            }))
            .hint("Alt+Enter"),
            Item::Separator,
            Item::Sub { label: t!("Diğer seçenekleri göster").into(), icon: MenuIcon::Lucide(L::Ellipsis), items: Sub::Pending { key: "more", fallback: None } },
        ];
        let menu = crate::fluent_menu::show(Menu { quick: Vec::new(), items }, window.mouse_position(), window, cx);
        let view = self.active_tab().downgrade();
        crate::ctx_menu::load_shell_more(menu.downgrade(), rf_win::ctxmenu::ShellTarget::One(path), view, cx);
    }

    /// Folders dropped on the sidebar's pin area get pinned (files are ignored, as in Explorer).
    fn pin_dropped(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        self.sidebar_ext_drag = false;
        let dirs: Vec<PathBuf> = paths.paths().iter().filter(|p| p.is_dir()).cloned().collect();
        if dirs.is_empty() {
            return;
        }
        let mut list = self.current_pins(cx);
        for d in dirs {
            if !list.contains(&d) {
                list.push(d);
            }
        }
        crate::settings::update(cx, |s| s.pinned = Some(list));
        self.places.update(cx, |p, cx| p.refresh(cx));
    }

    fn set_pinned(&mut self, path: PathBuf, pin: bool, cx: &mut Context<Self>) {
        let mut list = self.current_pins(cx);
        if !pin {
            list.retain(|p| *p != path);
        } else if !list.contains(&path) {
            list.push(path);
        }
        crate::settings::update(cx, |s| s.pinned = Some(list));
        self.places.update(cx, |p, cx| p.refresh(cx));
    }

    /// Moves tab `ix` into a new window (tab dragged onto the content area).
    fn detach_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 || ix >= self.tabs.len() {
            return;
        }
        let loc = self.tabs[ix].left.read(cx).location().clone();
        self.close_tab(ix, window, cx);
        let mut b = window.bounds();
        b.origin.x += px(48.);
        b.origin.y += px(48.);
        cx.defer(move |cx| crate::open_main_window(Some(loc), None, Some(WindowBounds::Windowed(b)), cx));
    }

    /// Right-click on Back/Forward: the history as a menu.
    fn show_history_menu(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.active_tab().clone();
        let titles: Vec<String> = {
            let v = tab.read(cx);
            let it: Box<dyn Iterator<Item = &Location>> =
                if back { Box::new(v.history.back_entries()) } else { Box::new(v.history.forward_entries()) };
            it.take(16)
                .map(|l| match l {
                    Location::Path(p) => p.to_string_lossy().into_owned(),
                    Location::DiskUsage(p) => crate::disk_usage::title(p),
                    other => crate::i18n::td(&other.title()),
                })
                .collect()
        };
        if titles.is_empty() {
            return;
        }
        use crate::fluent_menu::{Item, Menu, MenuIcon};
        let weak = tab.downgrade();
        let items: Vec<Item> = titles
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let weak = weak.clone();
                Item::action(t, MenuIcon::Lucide(L::Folder), move |_, cx| {
                    let _ = weak.update(cx, |v, cx| v.jump(back, i + 1, cx));
                })
            })
            .collect();
        crate::fluent_menu::show(Menu { quick: Vec::new(), items }, window.mouse_position(), window, cx);
    }

    fn nav_button(
        &self,
        id: &'static str,
        icon: L,
        enabled: bool,
        p: &Palette,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip_text(id)).build(w, cx))
            .size(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::RADIUS_SM)
            .text_color(if enabled { p.text } else { p.text_tertiary })
            .when(enabled, |d| {
                d.hover(|s| s.bg(p.hover))
                    .active(|s| s.bg(p.pressed))
                    .on_click(cx.listener(move |this, _, window, cx| f(this, window, cx)))
            })
            .child(Icon::new(icon).size(px(16.)))
            .into_any_element()
    }

    /// Navigation row: back / forward / up / refresh, the Omnibar (one box for path, search and
    /// commands, like Files) and the Status Center button.
    fn render_toolbar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let popups_open = self.popups_open(cx);
        let tab = self.active_tab().read(cx);
        let can_back = tab.history.can_back();
        let can_fwd = tab.history.can_forward();
        let loc = tab.location().clone();
        let can_up = loc.parent().is_some();
        // The Omnibar shows the search box only while it is being typed in; afterwards the path
        // comes back and a small chip keeps the active query visible (and clearable).
        let searching = self.search_mode;
        let live_query = self.search.read(cx).value().to_string();
        let active_query: Option<String> = match &loc {
            Location::Search { query, .. } => Some(query.clone()),
            _ => (!live_query.is_empty()).then_some(live_query),
        };

        let suggestions = self.render_suggestions(p, cx);
        let field: AnyElement = if searching {
            Input::new(&self.search).appearance(false).cleanable(true).into_any_element()
        } else if self.editing_address {
            Input::new(&self.address).appearance(false).into_any_element()
        } else {
            self.render_breadcrumb(&loc, p, cx).into_any_element()
        };
        let focused = self.editing_address || searching;
        let chip = active_query.filter(|_| !searching).map(|q| {
            h_flex()
                .id("search-chip")
                .flex_shrink_0()
                .max_w(px(220.))
                .h(px(24.))
                .pl_2()
                .gap_1()
                .items_center()
                .rounded(theme::RADIUS_SM)
                .bg(p.selection)
                .text_xs()
                .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Aramayı düzenle")).build(w, cx))
                .child(Icon::new(L::Search).size(px(12.)).text_color(p.accent))
                .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(q))
                .child(
                    div()
                        .id("search-chip-x")
                        .size(px(22.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::RADIUS_SM)
                        .hover(|s| s.bg(p.hover))
                        .child(Icon::new(L::X).size(px(11.)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, window, cx| this.exit_search(window, cx))),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| this.enter_search(window, cx)))
        });
        let mode_btn = |id: &'static str, icon: L, on: bool, tip: &'static str, cx: &mut Context<Self>, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            div()
                .id(id)
                .size(px(28.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .text_color(if on { p.accent } else { p.text_secondary })
                .when(on, |d| d.bg(p.selection))
                .hover(|s| s.bg(p.hover))
                .when(!popups_open, |d| d.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(crate::i18n::t(tip)).build(w, cx)))
                .child(Icon::new(icon).size(px(15.)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, window, cx| f(this, window, cx)))
        };
        let omnibar = h_flex()
            .id("omnibar")
            .flex_1()
            .min_w(px(160.))
            .h(px(36.))
            .mx_1()
            .pl_2()
            .pr_1()
            .gap_1()
            .items_center()
            // a capsule, like Files' Omnibar
            .rounded_full()
            .bg(p.content_bg)
            .border_1()
            .border_color(if focused { p.accent } else { p.border })
            .relative()
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if this.search_mode && ev.keystroke.key == "escape" {
                    this.exit_search(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if !this.search_mode {
                    this.address_key(ev, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &gpui_kit::component::input::IndentInline, window, cx| this.accept_suggestion(window, cx)))
            .when(searching, |d| d.child(Icon::new(L::Search).size(px(14.)).text_color(p.text_secondary)))
            .child(div().flex_1().min_w_0().overflow_hidden().child(field))
            .children(suggestions.filter(|_| !searching))
            .children(chip)
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(p.divider))
            .child(mode_btn("omni-cmd", L::SquareChevronRight, self.palette.is_some(), "Komut paleti  (Ctrl+Shift+P)", cx, |this, window, cx| {
                this.toggle_palette(window, cx)
            }))
            .child(mode_btn("omni-search", L::Search, searching, "Ara  (Ctrl+F)", cx, |this, window, cx| this.enter_search(window, cx)))
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.editing_address && !this.search_mode {
                    this.begin_address_edit(window, cx);
                }
            }));

        h_flex()
            .h(px(48.))
            .px_2()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_shrink_0()
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| this.show_history_menu(true, window, cx)))
                    .child(self.nav_button("back", L::ArrowLeft, can_back, p, cx, |this, _, cx| this.with_tab(cx, |v, cx| v.back(cx)))),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| this.show_history_menu(false, window, cx)))
                    .child(self.nav_button("fwd", L::ArrowRight, can_fwd, p, cx, |this, _, cx| this.with_tab(cx, |v, cx| v.forward(cx)))),
            )
            .child(self.nav_button("up", L::ArrowUp, can_up, p, cx, |this, _, cx| this.with_tab(cx, |v, cx| v.up(cx))))
            .child(self.nav_button("refresh", L::RotateCw, true, p, cx, |this, _, cx| this.with_tab(cx, |v, cx| v.refresh(cx))))
            .child(omnibar)
            .child(crate::status_center::render_button(&self.ops, p, cx))
    }

    fn enter_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_mode = true;
        self.editing_address = false;
        self.clear_suggestions();
        self.search.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn exit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_mode = false;
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        if let Some(tab) = self.tabs.get(self.active).map(|t| t.view().clone()) {
            tab.update(cx, |v, cx| {
                v.set_query(String::new(), cx);
                // leave a recursive search result back to its folder
                if let Location::Search { root, .. } = v.location().clone() {
                    v.navigate(Location::Path(root), cx);
                }
            });
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Command bar above the file list (Files' toolbar): create, clipboard and item commands on
    /// the left; sort, view and the preview pane on the right.
    fn render_commandbar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tab = self.active_tab().read(cx);
        let in_folder = tab.location().path().is_some();
        let (sel, _, _) = tab.selection_summary();
        let has_sel = sel > 0;
        // a phone's folder is read-only here: changing commands are greyed out, not refused later
        let writable_sel = has_sel && !matches!(tab.location(), Location::Shell { .. });
        let preview_open = self.preview_open;
        let filter_on = self.filter_bar;
        let paste_ok = in_folder && rf_win::clipboard::has_files();

        let cmd = |id: &'static str, icon: L, tip: &'static str, enabled: bool, cx: &mut Context<Self>, run: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            div()
                .id(id)
                .h(px(32.))
                .w(px(36.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::RADIUS_SM)
                .text_color(if enabled { p.text } else { p.text_tertiary })
                .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(crate::i18n::t(tip)).build(w, cx))
                .when(enabled, |d| {
                    d.hover(|s| s.bg(p.hover))
                        .active(|s| s.bg(p.pressed))
                        .on_click(cx.listener(move |this, _, window, cx| run(this, window, cx)))
                })
                .child(Icon::new(icon).size(px(16.)))
        };
        let popups_open = self.popups_open(cx);
        let drop = |id: &'static str, icon: L, label: Option<&'static str>, tip: &'static str, enabled: bool, cx: &mut Context<Self>, which: fn(&Self, &App) -> crate::ctx_menu::BarMenu| {
            h_flex()
                .id(id)
                .h(px(32.))
                .px_2()
                .gap_1()
                .flex_shrink_0()
                .items_center()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .text_color(if enabled { p.text } else { p.text_tertiary })
                .when(!popups_open, |d| d.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(crate::i18n::t(tip)).build(w, cx)))
                .child(Icon::new(icon).size(px(16.)))
                .children(label.map(|l| div().child(crate::i18n::t(l))))
                .child(Icon::new(L::ChevronDown).size(px(12.)).text_color(p.text_secondary))
                .when(enabled, |d| {
                    d.hover(|s| s.bg(p.hover)).active(|s| s.bg(p.pressed)).on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        let menu = which(this, cx);
                        // drop down from under the button (the bar's bottom edge)
                        let pos = point(ev.position().x - px(14.), theme::TITLEBAR_H + px(48. + 46.));
                        this.active_tab().clone().update(cx, |v, cx| v.show_bar_menu(menu, pos, window, cx));
                    }))
                })
        };
        let sep = || div().w(px(1.)).h(px(20.)).mx_1().bg(p.divider);

        h_flex()
            .h(px(44.))
            .px_2()
            .gap_0p5()
            .items_center()
            .border_b_1()
            .border_color(p.divider)
            .child(drop("cb-new", L::CirclePlus, Some("Yeni"), "Yeni öğe", in_folder, cx, |_, _| crate::ctx_menu::BarMenu::New))
            .child(sep())
            .child(cmd("cb-cut", L::Scissors, "Kes  (Ctrl+X)", writable_sel, cx, |this, w, cx| {
                this.with_tab(cx, |v, cx| v.copy_to_clipboard(rf_win::fileops::Transfer::Move, w, cx))
            }))
            .child(cmd("cb-copy", L::Copy, "Kopyala  (Ctrl+C)", has_sel, cx, |this, w, cx| {
                this.with_tab(cx, |v, cx| v.copy_to_clipboard(rf_win::fileops::Transfer::Copy, w, cx))
            }))
            .child(cmd("cb-paste", L::ClipboardPaste, "Yapıştır  (Ctrl+V)", paste_ok, cx, |this, w, cx| {
                this.with_tab(cx, |v, cx| v.paste(w, cx))
            }))
            .child(cmd("cb-rename", L::TextCursorInput, "Yeniden adlandır  (F2)", writable_sel, cx, |this, w, cx| {
                this.with_tab(cx, |v, cx| v.rename_selection(w, cx))
            }))
            .child(cmd("cb-share", L::Share2, "Paylaş", writable_sel, cx, |this, _, cx| this.with_tab(cx, |v, _| v.share())))
            .child(cmd("cb-delete", L::Trash, "Sil  (Del)", writable_sel, cx, |this, w, cx| {
                this.with_tab(cx, |v, cx| v.delete_selected(false, w, cx))
            }))
            .child(cmd("cb-props", L::Wrench, "Özellikler  (Alt+Enter)", in_folder, cx, |this, _, cx| {
                this.with_tab(cx, |v, cx| if v.selection_paths().is_empty() { v.folder_properties(cx) } else { v.properties(cx) })
            }))
            .child(div().flex_1())
            .child(
                div()
                    .id("cb-filter")
                    .h(px(32.))
                    .w(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Filtrele")).build(w, cx))
                    .when(filter_on, |d| d.bg(p.selection).text_color(p.accent))
                    .when(!filter_on, |d| d.hover(|s| s.bg(p.hover)))
                    .when(in_folder, |d| d.on_click(cx.listener(|this, _, window, cx| this.toggle_filter_bar(window, cx))))
                    .when(!in_folder, |d| d.text_color(p.text_tertiary))
                    .child(Icon::new(L::ListFilter).size(px(16.))),
            )
            .child(drop("cb-sort", L::ArrowUpDown, None, "Sırala ve gruplandır", in_folder, cx, |_, _| crate::ctx_menu::BarMenu::Sort))
            .child(drop("cb-view", L::LayoutGrid, None, "Görünüm", true, cx, move |this, _| crate::ctx_menu::BarMenu::View {
                dual: this.tabs[this.active].right.is_some(),
                shelf: this.shelf_open,
            }))
            .child(sep())
            .child(
                div()
                    .id("cb-preview")
                    .h(px(32.))
                    .w(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Önizleme bölmesi  (Alt+P)")).build(w, cx))
                    .when(preview_open, |d| d.bg(p.selection).text_color(p.accent))
                    .when(!preview_open, |d| d.hover(|s| s.bg(p.hover)))
                    .child(Icon::new(L::PanelRight).size(px(16.)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_preview(cx);
                    })),
            )
    }

    fn render_breadcrumb(&mut self, loc: &Location, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let drop_bg = p.selection;
        let mut row = h_flex().gap_0p5().items_center().text_sm().overflow_hidden();
        let crumbs: Vec<(String, Location)> = match loc {
            Location::Path(path) | Location::Search { root: path, .. } => breadcrumbs(path),
            // the shell's own name for the folder (the location may carry none)
            Location::Shell { .. } => vec![(self.active_tab().read(cx).title(cx), loc.clone())],
            Location::DiskUsage(path) => vec![(crate::disk_usage::title(path), loc.clone())],
            other => vec![(crate::i18n::td(&other.title()), other.clone())],
        };
        // Too long for the address box: hide leading segments after the drive behind "…"
        // (the current folder must stay visible). Widths are estimated from text length.
        let budget = self.viewport_w - 260. - 4. * 36. - 9. * 36. - 90.;
        let est = |l: &str| l.chars().count() as f32 * 7.4 + 30.;
        let mut crumbs = crumbs;
        let mut hidden: Option<Location> = None;
        while crumbs.len() > 2 && crumbs.iter().map(|(l, _)| est(l)).sum::<f32>() + 60. > budget {
            let (_, loc) = crumbs.remove(1);
            hidden = Some(loc);
        }
        let n = crumbs.len();
        let home_icon = div()
            .id("crumb-home")
            .px_1()
            .py_0p5()
            .rounded(theme::RADIUS_SM)
            .hover(|s| s.bg(p.hover))
            .child(Icon::new(L::House).size(px(14.)).text_color(p.text_secondary))
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.navigate(Location::Home, window, cx);
            }));
        row = row.child(home_icon);
        let chevron = |id: (&'static str, usize), dir: Option<PathBuf>, p: &Palette, cx: &mut Context<Self>| {
            div()
                .id(id)
                .px_0p5()
                .py_1()
                .rounded(theme::RADIUS_SM)
                .hover(|s| s.bg(p.hover))
                .child(Icon::new(L::ChevronRight).size(px(12.)).text_color(p.text_tertiary))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.crumb_menu(dir.clone(), window, cx);
                }))
        };
        let mut prev_dir: Option<PathBuf> = None;
        let last_dir = crumbs.last().and_then(|(_, t)| t.path().map(Path::to_path_buf));
        for (i, (label, target)) in crumbs.into_iter().enumerate() {
            if i == 1 {
                if let Some(h) = hidden.clone() {
                    // "…" stands for the hidden segments and opens the deepest of them
                    row = row.child(chevron(("crumb-chev-h", 0), prev_dir.clone(), p, cx));
                    prev_dir = h.path().map(Path::to_path_buf);
                    row = row.child(
                        div()
                            .id("crumb-ellipsis")
                            .px_1p5()
                            .py_0p5()
                            .rounded(theme::RADIUS_SM)
                            .text_color(p.text_secondary)
                            .hover(|s| s.bg(p.hover).text_color(p.text))
                            .child("…")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.navigate(h.clone(), window, cx);
                            })),
                    );
                }
            }
            row = row.child(chevron(("crumb-chev", i), prev_dir.clone(), p, cx));
            prev_dir = target.path().map(Path::to_path_buf);
            let last = i + 1 == n;
            row = row.child(
                div()
                    .id(("crumb", i))
                    .px_1p5()
                    .py_0p5()
                    .rounded(theme::RADIUS_SM)
                    .whitespace_nowrap()
                    .when(last, |d| d.font_weight(FontWeight::MEDIUM))
                    .when(!last, |d| d.text_color(p.text_secondary))
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child(label)
                    .when_some(target.path().map(Path::to_path_buf), |d, dir| {
                        d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
                            .on_drop(move |paths: &ExternalPaths, _, cx| drop_files(paths, dir.clone(), cx))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.navigate(target.clone(), window, cx);
                    })),
            );
        }
        if let Some(d) = last_dir {
            row = row.child(chevron(("crumb-chev", 999), Some(d), p, cx));
        }
        row
    }

    fn sidebar_item(
        &self,
        id: ElementId,
        icon: L,
        icon_color: Hsla,
        label: String,
        target: Location,
        active: bool,
        extra: Option<AnyElement>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let target2 = target.clone();
        let target3 = target.clone();
        let drop_dir = target.path().map(Path::to_path_buf);
        let drop_bg = p.selection;
        let drop_border = p.accent;
        // on the rail only the icon; hovering the rail opens the full sidebar over the content
        let rail = self.sidebar_rail;
        // drives look alike as bare icons: on the rail their letter sits in the icon's corner
        let letter = target3.path().filter(|_| rail).and_then(|p| {
            let s = p.to_str()?;
            let b = s.as_bytes();
            (b.len() <= 3 && b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()).then(|| (b[0] as char).to_ascii_uppercase())
        });
        h_flex()
            .id(id)
            .min_h(px(if rail { 36. } else { 32. }))
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .when(rail, |d| d.justify_center())
            .rounded(theme::RADIUS_SM)
            .text_sm()
            .relative()
            .when(active, |d| d.bg(p.selection).child(theme::selection_pill(p)))
            .when(!active, |d| d.hover(|s| s.bg(p.hover)))
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(Icon::new(icon).size(px(if rail { 18. } else { 16. })).text_color(icon_color))
                    .when_some(letter, |d, l| {
                        d.child(
                            div()
                                .absolute()
                                .right(px(-9.))
                                .bottom(px(-7.))
                                .min_w(px(15.))
                                .h(px(15.))
                                .px(px(2.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .bg(p.sidebar_bg)
                                .border_1()
                                .border_color(p.border)
                                .text_size(px(11.))
                                .line_height(px(13.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.text)
                                .child(l.to_string()),
                        )
                    }),
            )
            .when(!rail, |d| {
                d.child(
                    v_flex()
                        .flex_1()
                        .overflow_hidden()
                        .child(div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
                        .children(extra),
                )
            })

            .on_click(cx.listener(move |this, _, window, cx| this.navigate(target.clone(), window, cx)))
            .when_some(target3.path().map(Path::to_path_buf), |d, path| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.sidebar_menu(path.clone(), window, cx);
                    }),
                )
            })
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    let keep = this.active;
                    this.open_tab(target2.clone(), window, cx);
                    this.activate(keep, window, cx);
                }),
            )
            .when_some(drop_dir, |d, dir| {
                d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg).border_1().border_color(drop_border))
                    .on_drop(move |paths: &ExternalPaths, _, cx| drop_files(paths, dir.clone(), cx))
            })
    }

    fn render_sidebar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let cur = self.active_tab().read(cx).location().clone();
        let places = self.places.read(cx);
        let pinned = places.pinned.clone();
        let drives = places.drives.clone();
        let rail = self.sidebar_rail;
        let header = |t: &'static str| {
            if rail {
                return div().mx_2().my_2().h(px(1.)).bg(p.divider);
            }
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_tertiary)
                .child(t)
        };
        // the "drop here to pin" row lives as long as a file drag does
        if !cx.has_active_drag() {
            self.sidebar_ext_drag = false;
        }
        let ext_drag = self.sidebar_ext_drag;
        let (drop_bg, drop_border) = (p.selection, p.accent);
        let mut col = v_flex()
            .id("sidebar")
            .w_full()
            .flex_1()
            .min_h_0()
            .px_2()
            .py_1()
            .gap_0p5()
            .overflow_y_scroll()
            .on_drag_move::<ExternalPaths>(cx.listener(|this, _: &DragMoveEvent<ExternalPaths>, _, cx| {
                if !this.sidebar_ext_drag {
                    this.sidebar_ext_drag = true;
                    cx.notify();
                }
            }));
        col = col.child(self.sidebar_item(
            "sb-home".into(),
            L::House,
            p.accent,
            t!("Ana Sayfa").into(),
            Location::Home,
            cur == Location::Home,
            None,
            p,
            cx,
        ));
        // dropping folders on the header pins them
        col = col.child(
            div()
                .id("sb-pin-header")
                .rounded(theme::RADIUS_SM)
                .child(header(t!("Sabitlenenler")))
                .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.pin_dropped(paths, cx))),
        );
        let accent = p.accent;
        for (i, f) in pinned.into_iter().enumerate() {
            let loc = Location::Path(f.path.clone());
            let active = cur == loc;
            let title = crate::i18n::td(&f.title);
            let drag = PinDrag { ix: i, path: f.path.clone(), title: title.clone().into() };
            let target_path = f.path.clone();
            let item = self.sidebar_item(("sb-pin", i).into(), f.icon, p.accent, title, loc, active, None, p, cx);
            // drag a pin onto another to reorder
            col = col.child(
                div()
                    .id(("sb-pin-wrap", i))
                    .on_drag(drag, |d: &PinDrag, _, _, cx| cx.new(|_| d.clone()))
                    .drag_over::<PinDrag>(move |s, d: &PinDrag, _, _| {
                        // moving down inserts after the target, moving up before it
                        if d.ix < i { s.border_b_2().border_color(accent) } else { s.border_t_2().border_color(accent) }
                    })
                    .on_drop(cx.listener(move |this, d: &PinDrag, _, cx| this.move_pin(&d.path, &target_path, cx)))
                    .child(item),
            );
        }
        if ext_drag {
            col = col.child(
                h_flex()
                    .id("sb-pin-drop")
                    .min_h(px(32.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .border_1()
                    .border_dashed()
                    .border_color(p.divider)
                    .text_sm()
                    .text_color(p.text_tertiary)
                    .child(Icon::new(L::Pin).size(px(16.)).flex_shrink_0())
                    .when(!rail, |d| d.child(t!("Sabitlemek için buraya bırakın")))
                    .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg).border_color(drop_border))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.pin_dropped(paths, cx))),
            );
        }
        let hidden = cx.global::<crate::settings::Settings>().sidebar_hidden.clone();
        let shown = |k: &str| !hidden.iter().any(|h| h == k);
        let drives = if shown("drives") { drives } else { Vec::new() };
        if !drives.is_empty() {
            col = col.child(header(t!("Sürücüler")));
        }
        for (i, d) in drives.into_iter().enumerate() {
            let loc = Location::Path(d.root.clone());
            let active = cur == loc;
            let frac = d.used_fraction();
            let bar = (d.total > 0).then(|| {
                div()
                    .mt(px(5.))
                    .h(px(2.))
                    .w_full()
                    .rounded_full()
                    .bg(p.pressed)
                    .child(div().h_full().rounded_full().bg(if frac > 0.9 { p.danger } else { p.accent }).w(relative(frac)))
                    .into_any_element()
            });
            col = col.child(self.sidebar_item(
                ("sb-drive", i).into(),
                places::drive_icon(&d),
                p.text_secondary,
                crate::places::drive_name(&d),
                loc,
                active,
                bar,
                p,
                cx,
            ));
        }
        let cloud = self.places.read(cx).cloud.clone();
        if !cloud.is_empty() && shown("cloud") {
            col = col.child(header(t!("Bulut ve Linux")));
            for (i, c) in cloud.into_iter().enumerate() {
                let loc = Location::Path(c.path.clone());
                let active = cur == loc;
                col = col.child(self.sidebar_item(("sb-cloud", i).into(), c.icon, p.accent, crate::i18n::td(&c.title), loc, active, None, p, cx));
            }
        }
        let libraries = self.places.read(cx).libraries.clone();
        // Libraries duplicate the pinned folders: off unless turned on in Settings
        if !libraries.is_empty() && cx.global::<crate::settings::Settings>().show_libraries {
            col = col.child(header(t!("Kütüphaneler")));
            for (i, l) in libraries.into_iter().enumerate() {
                let loc = Location::Library { file: l.file.clone(), name: l.name.clone() };
                let active = cur == loc;
                col = col.child(self.sidebar_item(("sb-lib", i).into(), L::Library, p.accent, l.name, loc, active, None, p, cx));
            }
        }
        let network = self.places.read(cx).network.clone();
        if !network.is_empty() && shown("network") {
            col = col.child(header(t!("Ağ")));
            for (i, (name, target)) in network.into_iter().enumerate() {
                let loc = Location::Path(target);
                let active = cur == loc;
                col = col.child(self.sidebar_item(("sb-net", i).into(), L::Network, p.text_secondary, name, loc, active, None, p, cx));
            }
        }
        let tag_defs = if shown("tags") { cx.global::<crate::tags::Tags>().0.defs.clone() } else { Vec::new() };
        if !tag_defs.is_empty() {
            col = col.child(header(t!("Etiketler")));
        }
        for d in tag_defs {
            let loc = Location::Tag(d.id);
            let active = cur == loc;
            col = col.child(self.sidebar_item(
                ("sb-tag", d.id as usize).into(),
                L::Tag,
                crate::tags::color(d.color),
                crate::i18n::td(&d.name),
                loc,
                active,
                None,
                p,
                cx,
            ));
        }
        col = col.child(div().h_2());
        col = col.child(self.sidebar_item(
            "sb-recycle".into(),
            L::Trash,
            p.text_secondary,
            t!("Geri Dönüşüm Kutusu").into(),
            Location::RecycleBin,
            cur == Location::RecycleBin,
            None,
            p,
            cx,
        ));
        let settings = self.sidebar_item(
            "sb-settings".into(),
            L::Settings,
            p.text_secondary,
            t!("Ayarlar").into(),
            Location::Settings,
            cur == Location::Settings,
            None,
            p,
            cx,
        );
        // Files: Settings stays at the bottom of the sidebar, under a divider
        v_flex()
            .w(px(if self.sidebar_rail { RAIL_W } else { self.sidebar_w }))
            .h_full()
            .child(col)
            .child(div().mx_2().h(px(1.)).bg(p.divider))
            .child(div().px_2().py_1().child(settings))
    }

    fn render_statusbar(&mut self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tab = self.active_tab().read(cx);
        let count = tab.item_count();
        let (sel, sel_size, has_dir) = tab.selection_summary();
        let loading = tab.loading;
        let ms = tab.load_ms;
        let in_folder = tab.location().path().is_some() || matches!(tab.location(), Location::Tag(_) | Location::Library { .. });
        let git = tab.git.clone();
        // Settings → Developer tools → "Open in editor" button
        let ide_button = {
            let prefs = cx.global::<crate::settings::Settings>();
            let show = match prefs.ide_button {
                crate::settings::IdePref::Always => true,
                crate::settings::IdePref::GitRepos => git.is_some(),
                crate::settings::IdePref::Never => false,
            };
            let dir = tab.location().path().map(Path::to_path_buf);
            let ide = prefs.ide_path.clone();
            (show && dir.is_some()).then(|| {
                let dir = dir.unwrap();
                h_flex()
                    .id("open-ide")
                    .h(px(22.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Klasörü düzenleyicide aç")).build(w, cx))
                    .child(Icon::new(L::Code).size(px(12.)))
                    .child(t!("Aç"))
                    .on_click(move |_, _, _| open_in_ide(dir.clone(), ide.clone()))
            })
        };
        let mut left = h_flex().gap_3();
        if in_folder {
            left = left.child(crate::i18n::items(count));
            if sel > 0 {
                let s = if has_dir || sel_size == 0 {
                    crate::i18n::items_selected(sel)
                } else {
                    format!("{} · {}", crate::i18n::items_selected(sel), format::size(sel_size))
                };
                left = left.child(div().text_color(p.text).child(s));
            }
            if loading {
                left = left.child(t!("Yükleniyor…"));
            }
        }
        let perf = cx.global::<crate::settings::Settings>().show_perf_info;
        h_flex()
            .h(px(32.))
            .flex_shrink_0()
            .px_3()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(p.divider)
            .text_xs()
            .text_color(p.text_secondary)
            .child(left)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .children(git.map(|g| {
                        // Files-style git controls: ↑↓ (sync menu) and the branch (branch menu)
                        let view = self.active_tab().downgrade();
                        let (root, ahead, behind) = (g.root.clone(), g.ahead, g.behind);
                        let pill = |id: &'static str| {
                            h_flex()
                                .id(id)
                                .h(px(22.))
                                .px_2()
                                .gap_1()
                                .items_center()
                                .rounded(theme::RADIUS_SM)
                                .hover(|s| s.bg(p.hover).text_color(p.text))
                                .active(|s| s.bg(p.pressed))
                        };
                        let (v1, r1) = (view.clone(), root.clone());
                        let changes = (!g.is_clean()).then(|| {
                            div().text_color(p.text_tertiary).child(format!("{} {}", g.files.len(), t!("değişiklik")))
                        });
                        h_flex()
                            .gap_1()
                            .items_center()
                            .children(changes)
                            .child(
                                pill("git-sync")
                                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Çek / Gönder / Eşitle")).build(w, cx))
                                    .child(Icon::new(L::ArrowDownUp).size(px(12.)))
                                    .child(format!("{behind} / {ahead}"))
                                    .on_click(move |ev: &ClickEvent, window, cx| {
                                        crate::git_ui::sync_menu(v1.clone(), r1.clone(), ahead, behind, ev.position(), window, cx)
                                    }),
                            )
                            .child(
                                pill("git-branch")
                                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Dallar")).build(w, cx))
                                    .child(Icon::new(L::GitBranch).size(px(12.)))
                                    .child(g.branch.clone())
                                    .on_click(move |ev: &ClickEvent, window, cx| {
                                        crate::git_ui::branch_menu(view.clone(), root.clone(), ev.position(), window, cx)
                                    }),
                            )
                    }))
                    .children(ide_button)
                    // a downloaded update: one click restarts into it
                    .children(matches!(crate::installer::state(), crate::installer::State::Ready(_)).then(|| {
                        h_flex()
                            .id("update-ready")
                            .h(px(22.))
                            .px_2()
                            .gap_1()
                            .items_center()
                            .rounded(theme::RADIUS_SM)
                            .text_color(p.accent)
                            .hover(|s| s.bg(p.hover))
                            .child(Icon::new(L::RefreshCw).size(px(12.)))
                            .child(t!("Güncelleme hazır"))
                            .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Yeniden başlat ve güncelle")).build(w, cx))
                            .on_click(|_, _, _| {
                                std::thread::spawn(crate::installer::restart_to_update);
                            })
                    }))
                    .children(ms.filter(|_| in_folder && perf).map(|ms| format!("{ms} ms")))
                    .children(perf.then(|| render_backend_badge(p))),
            )
    }
}

/// Title bar: width of the leading corner button, tab width range.
const TITLEBAR_LEAD: f32 = 32.;
/// New tab: the "+" box grows to a tab, then the next "+" slides out from behind it.
const TAB_ANIM_MS: u64 = 200;
const PLUS_ANIM_MS: u64 = 160;
/// Dragged files held over a tab this long open it (Explorer uses about the same).
const DRAG_HOVER_MS: u64 = 500;
const TAB_MIN_W: f32 = 72.;
const TAB_MAX_W: f32 = 220.;
const SIDEBAR_MIN: f32 = 160.;
/// The icons-only sidebar, and the width below which a drag collapses to it.
const RAIL_W: f32 = 56.;
const RAIL_SNAP: f32 = 120.;
const SIDEBAR_MAX: f32 = 420.;
const PREVIEW_MIN: f32 = 220.;
const PREVIEW_MAX: f32 = 720.;

/// Drag payloads for the pane edges.
#[derive(Clone)]
struct ResizeSidebar;
#[derive(Clone)]
struct ResizePreview;

struct ResizeGhost;
impl Render for ResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A thin invisible strip on a pane edge (`left` = on the pane's left edge) that starts a resize.
fn resize_handle<T: Clone + 'static>(id: &'static str, payload: T, left: bool) -> impl IntoElement {
    div()
        .id(id)
        .absolute()
        .top_0()
        .h_full()
        .w(px(6.))
        .when(left, |d| d.left(px(-3.)))
        // the sidebar's right edge: stay inside it, the content card is painted on top
        .when(!left, |d| d.right_0())
        .cursor_col_resize()
        .on_drag(payload, |_, _, _, cx| cx.new(|_| ResizeGhost))
}

/// Toolbar tooltips (with shortcuts).
fn tooltip_text(id: &str) -> &'static str {
    match id {
        "back" => t!("Geri  (Alt+←)"),
        "fwd" => t!("İleri  (Alt+→)"),
        "up" => t!("Üst klasör  (Alt+↑)"),
        "refresh" => t!("Yenile  (F5)"),
        "hidden" => t!("Gizli öğeleri göster/gizle  (Ctrl+H)"),
        "terminal" => t!("Terminalde aç"),
        "sort" => t!("Sıralama ve gruplama"),
        "v-details" => t!("Detaylar  (Ctrl+Shift+1)"),
        "v-grid" => t!("Izgara  (Ctrl+Shift+2)"),
        "v-cols" => t!("Sütunlar  (Ctrl+Shift+3)"),
        "v-dual" => t!("Çift panel  (Ctrl+Shift+D)"),
        "v-preview" => t!("Önizleme paneli  (Alt+P)"),
        "v-shelf" => t!("Raf"),
        _ => "",
    }
}

/// Drop onto a sidebar entry / breadcrumb segment: Explorer rules (same volume = move).
/// Opens `dir` in the user's editor (`ide` path, else VS Code's `code` launcher).
pub fn open_in_ide(dir: PathBuf, ide: String) {
    std::thread::spawn(move || {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let r = if ide.is_empty() {
            // `code` is a .cmd launcher: run it through cmd without a console window
            std::process::Command::new("cmd").args(["/c", "code", "."]).current_dir(&dir).creation_flags(CREATE_NO_WINDOW).spawn()
        } else {
            std::process::Command::new(&ide).arg(&dir).spawn()
        };
        if let Err(e) = r {
            log::warn!("open in editor: {e}");
        }
    });
}

fn drop_files(paths: &ExternalPaths, dest: PathBuf, cx: &mut App) {
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

/// Shows which device draws the window (hybrid renderer) and cycles Auto → GPU → CPU on click.
fn render_backend_badge(p: &Palette) -> impl IntoElement + use<> {
    use gpui_windows::hybrid::{self, RendererMode};
    let mode = hybrid::mode();
    let backend = hybrid::current_backend_label();
    let mode_label = match mode {
        RendererMode::Auto => t!("Oto"),
        RendererMode::Gpu => t!("Sabit"),
        RendererMode::Cpu => t!("Sabit"),
    };
    let is_cpu = backend == "CPU";
    h_flex()
        .id("renderer-badge")
        .px_2()
        .py_0p5()
        .gap_1()
        .items_center()
        .rounded_full()
        .bg(p.hover)
        .hover(|s| s.bg(p.pressed))
        .cursor_pointer()
        .child(Icon::new(if is_cpu { L::Cpu } else { L::Gauge }).size(px(12.)).text_color(if is_cpu { p.danger } else { p.accent }))
        .child(format!("{backend} · {mode_label}"))
        .on_click(|_, _, cx| {
            use crate::settings::RendererPref;
            let next = match hybrid::mode() {
                RendererMode::Auto => RendererPref::Gpu,
                RendererMode::Gpu => RendererPref::Cpu,
                RendererMode::Cpu => RendererPref::Auto,
            };
            crate::settings::update(cx, |s| s.renderer = next);
        })
}

/// "C:\Users\x" → [("C:", C:\), ("Users", C:\Users), ("x", C:\Users\x)]
fn breadcrumbs(path: &Path) -> Vec<(String, Location)> {
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for comp in path.components() {
        acc.push(comp.as_os_str());
        if matches!(comp, std::path::Component::RootDir) {
            // merge the root separator into the drive crumb
            if let Some(last) = out.last_mut() {
                let (_, loc): &mut (String, Location) = last;
                *loc = Location::Path(acc.clone());
            }
            continue;
        }
        let label = match rf_win::localized::folder_label(&acc, None) {
            Some(l) => l.to_string(),
            None => rf_core::history::redact_credentials(comp.as_os_str().to_string_lossy().trim_end_matches('\\')),
        };
        out.push((label, Location::Path(acc.clone())));
    }
    out
}

fn parse_location(text: &str) -> Option<Location> {
    let t = text.trim().trim_matches('"');
    if t.is_empty() {
        return None;
    }
    let lower = t.to_lowercase();
    if lower == "home" || lower == "ana sayfa" {
        return Some(Location::Home);
    }
    // Environment variables like %USERPROFILE%
    let expanded = expand_env(t);
    let mut p = PathBuf::from(&expanded);
    if expanded.starts_with(crate::aliases::PREFIX) {
        // "@name\sub": a folder alias; unknown names fall through as a plain path
        if let Some(r) = crate::aliases::resolve(&expanded) {
            p = PathBuf::from(r);
        }
    }
    if expanded.len() == 2 && expanded.ends_with(':') {
        p = PathBuf::from(format!("{expanded}\\"));
    }
    Some(Location::Path(p))
}

fn expand_env(s: &str) -> String {
    if let Some(r) = crate::aliases::resolve(s) {
        return r;
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('%') {
            let name = &after[..end];
            match std::env::var(name) {
                Ok(v) => out.push_str(&v),
                Err(_) => {
                    out.push('%');
                    out.push_str(name);
                    out.push('%');
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        {
            static FIRST: std::sync::Once = std::sync::Once::new();
            FIRST.call_once(|| crate::start_mark("first render"));
        }
        // Shortcuts (Ctrl+T, Ctrl+W, …) only reach elements on the focus path: with nothing
        // focused (a window opened from a desktop icon, a closed popup took focus with it) they
        // would do nothing. The current folder takes focus back.
        if window.focused(cx).is_none() && !self.tabs.is_empty() {
            self.focus_active(window, cx);
        }
        let p = Palette::for_window(window);
        // Mica / frame follow the app's theme, not only the system one (re-applied when either
        // changes: GPUI resets it on a system theme switch)
        let prefs = cx.global::<crate::settings::Settings>();
        let (backdrop, tint, bg_image, bg_opacity, show_toolbar, show_statusbar) = (
            prefs.backdrop,
            prefs.tint,
            prefs.background_image.clone(),
            prefs.background_opacity,
            prefs.show_toolbar,
            prefs.show_statusbar,
        );
        // Home and Settings have nothing to count: no empty strip at the bottom
        let show_statusbar = show_statusbar && !matches!(self.active_tab().read(cx).location(), Location::Home | Location::Settings);
        if self.applied_backdrop != Some(backdrop) {
            self.applied_backdrop = Some(backdrop);
            use crate::settings::BackdropPref;
            window.set_background_appearance(match backdrop {
                BackdropPref::Mica => WindowBackgroundAppearance::MicaBackdrop,
                BackdropPref::MicaAlt => WindowBackgroundAppearance::MicaAltBackdrop,
                BackdropPref::Acrylic => WindowBackgroundAppearance::Blurred,
                // opaque surface painted by the palette; keep Mica under it so a switch back is instant
                BackdropPref::Solid => WindowBackgroundAppearance::MicaBackdrop,
            });
        }
        // background color tint and picture (under everything)
        let mut backdrop_layers: Vec<AnyElement> = Vec::new();
        if let Some(c) = tint {
            let mut h: Hsla = rgb(c).into();
            h.a = if p.dark { 0.22 } else { 0.16 };
            backdrop_layers.push(div().absolute().inset_0().bg(h).into_any_element());
        }
        if let Some(path) = bg_image {
            backdrop_layers.push(
                img(path)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .opacity(bg_opacity.clamp(0.05, 0.6))
                    .into_any_element(),
            );
        }
        let frame_key = (p.dark, window.appearance());
        if self.frame_dark != Some(frame_key) {
            self.frame_dark = Some(frame_key);
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = window.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    rf_win::window::set_dark_mode(w.hwnd.get(), p.dark);
                }
            }
        }
        self.viewport_w = f32::from(window.viewport_size().width);
        let titlebar = self.render_titlebar(&p, cx);
        let toolbar = self.render_toolbar(&p, cx);
        let commandbar = self.render_commandbar(&p, cx);
        let filter_strip = self.render_filter_bar(&p, window, cx);
        // the command bar acts on folder contents: not on Home, Settings, the Recycle Bin…
        let show_commandbar = matches!(self.active_tab().read(cx).location(), Location::Path(_) | Location::Search { .. } | Location::Library { .. } | Location::Tag(_) | Location::Shell { .. });
        // Adaptive layout (as Files does): a narrow window gives the list its room first by
        // hiding the preview, then the sidebar. The settings themselves are not changed.
        const LIST_MIN: f32 = 380.;
        let side_w = if self.sidebar_rail { RAIL_W } else { self.sidebar_w };
        let show_sidebar = self.sidebar_open && self.compact.is_none() && self.viewport_w >= side_w + LIST_MIN;
        let room_for_preview = self.viewport_w >= self.preview_w + LIST_MIN + if show_sidebar { side_w } else { 0. };
        let sidebar = show_sidebar.then(|| self.render_sidebar(&p, cx));
        let peek = (show_sidebar && self.sidebar_rail && (self.rail_peek[0] || self.rail_peek[1])).then(|| {
            self.sidebar_rail = false;
            let full = self.render_sidebar(&p, cx);
            self.sidebar_rail = true;
            div()
                .id("rail-peek")
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(px(self.sidebar_w))
                .bg(self.rail_peek_bg.unwrap_or(Hsla { a: 1., ..p.content_bg }))
                .border_r_1()
                .border_color(p.border)
                .rounded_r(theme::RADIUS)
                .shadow_lg()
                .on_hover(cx.listener(|this, h: &bool, _, cx| {
                    this.rail_peek[1] = *h;
                    cx.notify();
                }))
                .child(full)
        });
        let status = self.render_statusbar(&p, cx);
        let picker_footer = self.picker.clone().map(|pk| crate::picker::footer(self, &pk, &p, window, cx));
        let star_strip = (self.picker.is_none() && crate::installer::star_due(cx.global::<crate::settings::Settings>())).then(|| {
            let action = |id: &'static str, label: &'static str, star: bool| {
                div()
                    .id(id)
                    .px_2()
                    .py_0p5()
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .when(star, |d| d.text_color(p.accent))
                    .hover(|s| s.bg(p.hover))
                    .child(label)
                    .on_click(move |_, _, cx| {
                        if star {
                            std::thread::spawn(|| {
                                let _ = rf_win::shell::open(std::path::Path::new(crate::installer::REPO));
                            });
                        }
                        crate::settings::update(cx, |s| s.star_asked = true);
                    })
            };
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(p.divider)
                .text_xs()
                .text_color(p.text_secondary)
                .child(Icon::new(L::Star).size(px(12.)).text_color(p.accent))
                .child(div().flex_1().child(t!("smoothyfiles işinize yarıyor mu? GitHub'da bir yıldız, başkalarının da bulmasına yardım eder.")))
                .child(action("star-yes", t!("Yıldız ver"), true))
                .child(action("star-no", t!("Bir daha gösterme"), false))
        });
        // now and then, a note from the author; its message changes each time it is answered
        let donate_strip = (self.picker.is_none() && crate::installer::donate_due(cx.global::<crate::settings::Settings>())).then(|| {
            let s = cx.global::<crate::settings::Settings>();
            let all = crate::donate::messages(s.starts);
            let message = all[s.donate_seen as usize % all.len()].clone();
            let action = |id: &'static str, label: &'static str, kind: u8| {
                div()
                    .id(id)
                    .px_2()
                    .py_0p5()
                    .flex_shrink_0()
                    .rounded(theme::RADIUS_SM)
                    .cursor_pointer()
                    .when(kind == 0, |d| d.text_color(p.accent))
                    .hover(|s| s.bg(p.hover))
                    .child(label)
                    .on_click(move |_, _, cx| {
                        if kind == 0 {
                            std::thread::spawn(|| {
                                let _ = rf_win::shell::open(std::path::Path::new(crate::installer::DONATE));
                            });
                        }
                        crate::settings::update(cx, |s| match kind {
                            0 => crate::installer::donate_answered(s, true),
                            1 => crate::installer::donate_answered(s, false),
                            _ => s.donate_never = true,
                        });
                    })
            };
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(p.divider)
                .text_xs()
                .text_color(p.text_secondary)
                .child(Icon::new(L::Coffee).size(px(12.)).text_color(p.accent))
                .child(div().flex_1().min_w_0().child(message))
                .child(action("coffee-yes", t!("Kahve ısmarla"), 0))
                .child(action("coffee-later", t!("Sonra"), 1))
                .child(action("coffee-no", t!("Bir daha gösterme"), 2))
        });
        // Taskbar title follows the active folder.
        let title = format!("{} — smoothyfiles", self.active_tab().read(cx).title(cx));
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }
        // Keep the saved session current (so it survives a crash or forced shutdown).
        let session: Vec<String> = self
            .tabs
            .iter()
            .map(|t| session_text(t.left.read(cx).location()))
            .collect();
        if session != self.last_session && self.picker.is_none() {
            self.last_session = session.clone();
            let active = self.active;
            cx.defer(move |cx| {
                cx.update_global::<crate::settings::Settings, _>(|s, _| {
                    s.session_tabs = session;
                    s.session_active = active;
                });
                let snapshot = cx.global::<crate::settings::Settings>().clone();
                std::thread::spawn(move || snapshot.save());
            });
        }
        let tab_state = self.tabs[self.active].clone();
        let content: AnyElement = match &tab_state.right {
            None => div().size_full().child(tab_state.left.clone()).into_any_element(),
            Some(right) => {
                let side_by_side = cx.global::<crate::settings::Settings>().dual_pane_vertical;
                let pane = |view: Entity<FolderView>, is_right: bool, active: bool, cx: &mut Context<Self>| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .when(side_by_side, |d| d.h_full())
                        .when(!side_by_side, |d| d.w_full())
                        .relative()
                        .capture_any_mouse_down(cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.set_active_pane(is_right, cx)
                        }))
                        .child(view)
                        .when(active, |d| {
                            d.child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(p.accent))
                        })
                };
                let left = pane(tab_state.left.clone(), false, !tab_state.right_active, cx);
                let right = pane(right.clone(), true, tab_state.right_active, cx);
                if side_by_side {
                    h_flex().size_full().child(left).child(div().w(px(1.)).h_full().bg(p.divider)).child(right).into_any_element()
                } else {
                    v_flex().size_full().child(left).child(div().h(px(1.)).w_full().bg(p.divider)).child(right).into_any_element()
                }
            }
        };
        let shelf = self.shelf_open.then(|| {
            div()
                .w(px(260.))
                .flex_shrink_0()
                .h_full()
                .border_l_1()
                .border_color(p.divider)
                .child(self.shelf.clone())
        });
        let pane_shown = self.preview_open && self.compact.is_none() && room_for_preview && self.active_tab().read(cx).location().path().is_some();
        // the hosted preview handler is a separate window: hide it whenever something of ours
        // could be under it (pane closed, a dialog or the Status Center open, palette, bulk rename)
        let overlay_open = crate::prompt::current(window, cx).is_some() || crate::conflict_dialog::current(window, cx).is_some() || self.ops.read(cx).open || self.palette.is_some() || self.bulk.is_some();
        let covered = !pane_shown || overlay_open || self.quick_look.is_some();
        self.preview.update(cx, |pv, _| pv.set_occluded(covered));
        // Hızlı Bakış penceresi uygulamadan açıldıysa odaktaki öğeyi izler (oklar burada çalışır)
        let look_open = crate::folder_view::QUICK_LOOK_OPEN.load(std::sync::atomic::Ordering::Relaxed);
        if look_open && window.is_window_active() {
            if let Some((path, e)) = self.active_tab().read(cx).preview_target() {
                let mut last = LOOK_FOLLOWED.lock().unwrap();
                if last.as_ref() != Some(&path) {
                    *last = Some(path.clone());
                    crate::archive_view::look_send(path, e.size, true);
                }
            }
        }
        // Quick Look follows the focused item (arrow keys keep working underneath)
        let ql_target = self.quick_look.as_ref().and_then(|_| self.active_tab().read(cx).preview_target());
        if self.quick_look.is_some() && ql_target.is_none() {
            self.close_quick_look(cx);
        }
        let quick_look_layer = self.quick_look.clone().map(|ql| {
            ql.update(cx, |pv, cx| {
                pv.set_occluded(overlay_open);
                pv.set_target(ql_target, cx);
            });
            div()
                .id("quick-look")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.45))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close_quick_look(cx)))
                .child(
                    div()
                        .id("quick-look-card")
                        .w(relative(0.8))
                        .h(relative(0.82))
                        .p_4()
                        .rounded(theme::RADIUS)
                        .bg(p.card_bg)
                        .border_1()
                        .border_color(p.border)
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(ql)
                        .with_animation("ql-in", Animation::new(std::time::Duration::from_millis(140)).with_easing(ease_out_quint()), |el, t| {
                            el.opacity(t)
                        }),
                )
        });
        let preview = if pane_shown {
            let target = self.active_tab().read(cx).preview_target();
            self.preview.update(cx, |pv, cx| pv.set_target(target, cx));
            Some(
                div()
                    .w(px(self.preview_w))
                    .flex_shrink_0()
                    .h_full()
                    .relative()
                    .border_l_1()
                    .border_color(p.divider)
                    .child(self.preview.clone())
                    .child(resize_handle("preview-resize", ResizePreview, true)),
            )
        } else {
            None
        };

        v_flex()
            .id("workspace")
            .size_full()
            .bg(p.window_bg)
            .text_color(p.text)
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.with_tab(cx, |v, cx| v.back(cx))))
            // Windows' own dialogs (credentials, reconnect at sign-in); the drive list notices
            // the new / removed drive by itself
            .on_action(cx.listener(|_, _: &MapNetworkDrive, window, _| {
                rf_win::network::map_drive_dialog(Self::hwnd(window).unwrap_or(0))
            }))
            .on_action(cx.listener(|_, _: &DisconnectNetworkDrive, window, _| {
                rf_win::network::disconnect_drive_dialog(Self::hwnd(window).unwrap_or(0))
            }))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.with_tab(cx, |v, cx| v.forward(cx))))
            .on_action(cx.listener(|this, _: &GoUp, _, cx| this.with_tab(cx, |v, cx| v.up(cx))))
            .on_action(cx.listener(|this, _: &GoHome, window, cx| this.navigate(Location::Home, window, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.with_tab(cx, |v, cx| v.refresh(cx))))
            .on_action(cx.listener(|this, _: &ToggleHidden, _, cx| this.with_tab(cx, |v, cx| v.toggle_hidden(cx))))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.open_tab(Location::Home, window, cx)))
            .on_action(cx.listener(|this, _: &NewWindow, window, cx| {
                // New window at the current folder, offset from this one.
                let loc = this.active_tab().read(cx).location().clone();
                let mut b = window.bounds();
                b.origin.x += px(36.);
                b.origin.y += px(36.);
                cx.defer(move |cx| crate::open_main_window(Some(loc), None, Some(WindowBounds::Windowed(b)), cx));
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let i = this.active;
                this.close_tab(i, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                let n = this.tabs.len();
                this.activate((this.active + 1) % n, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                let n = this.tabs.len();
                this.activate((this.active + n - 1) % n, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                if let Some(loc) = this.closed.pop() {
                    this.open_tab(loc, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| this.begin_address_edit(window, cx)))
            .on_action(cx.listener(|this, _: &TogglePreview, _, cx| {
                this.toggle_preview(cx);
            }))
            .on_action(cx.listener(|this, _: &DuplicateTab, window, cx| {
                let loc = this.active_tab().read(cx).location().clone();
                this.open_tab(loc, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseOtherTabs, window, cx| this.close_other_tabs(window, cx)))
            .on_action(cx.listener(|_, _: &ToggleTheme, window, cx| {
                use crate::settings::ThemePref;
                let dark = crate::theme::Palette::for_window(window).dark;
                crate::settings::update(cx, |s| s.theme = if dark { ThemePref::Light } else { ThemePref::Dark });
            }))
            .on_action(cx.listener(|this, _: &OpenInIde, _, cx| {
                if let Some(dir) = this.active_tab().read(cx).location().path().map(Path::to_path_buf) {
                    let ide = cx.global::<crate::settings::Settings>().ide_path.clone();
                    open_in_ide(dir, ide);
                }
            }))
            .on_action(cx.listener(|_, _: &ToggleToolbar, _, cx| {
                let v = !cx.global::<crate::settings::Settings>().show_toolbar;
                crate::settings::update(cx, |s| s.show_toolbar = v);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                let v = !this.sidebar_open;
                crate::settings::update(cx, |s| s.sidebar_open = v);
            }))
            .on_action(cx.listener(|this, _: &ToggleDualPane, window, cx| this.toggle_dual_pane(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleShelf, _, cx| this.toggle_shelf(cx)))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &ToggleAlwaysOnTop, window, _| this.toggle_always_on_top(window)))
            .on_action(cx.listener(|this, _: &ToggleCompactOverlay, window, cx| this.toggle_compact(window, cx)))
            .on_action(cx.listener(|this, _: &GoToFolderAlias, window, cx| {
                // Files: the address box opens with "@" and lists the aliases
                this.begin_address_edit(window, cx);
                this.address.update(cx, |s, cx| s.set_value("@", window, cx));
                this.update_suggestions("@".into(), cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| this.toggle_palette(window, cx)))
            .on_action(cx.listener(|this, _: &GoSettings, window, cx| this.navigate(Location::Settings, window, cx)))
            .on_action(cx.listener(|this, _: &GoRecycleBin, window, cx| this.navigate(Location::RecycleBin, window, cx)))
            .on_action(cx.listener(|this, _: &SwitchPane, window, cx| {
                let r = !this.tabs[this.active].right_active;
                this.set_active_pane(r, cx);
                this.focus_active(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ViewDetails, _, cx| this.with_tab(cx, |v, cx| v.set_view_mode(ViewMode::Details, cx))))
            .on_action(cx.listener(|this, _: &ViewGrid, _, cx| this.with_tab(cx, |v, cx| v.set_view_mode(ViewMode::Grid, cx))))
            .on_action(cx.listener(|this, _: &ViewColumns, _, cx| this.with_tab(cx, |v, cx| v.set_view_mode(ViewMode::Columns, cx))))
            .on_action(cx.listener(|this, _: &ViewCards, _, cx| {
                this.with_tab(cx, |v, cx| v.set_layout(ViewMode::Grid, crate::folder_view::GridStyle::Cards, cx))
            }))
            .on_action(cx.listener(|this, _: &ViewList, _, cx| {
                this.with_tab(cx, |v, cx| v.set_layout(ViewMode::Grid, crate::folder_view::GridStyle::List, cx))
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.enter_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenTerminal, _, cx| {
                if let Some(dir) = this.active_tab().read(cx).location().path().map(Path::to_path_buf) {
                    std::thread::spawn(move || {
                        let _ = rf_win::shell::open_terminal(&dir);
                    });
                }
            }))
            .relative()
            .children(backdrop_layers)
            .child(titlebar)
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ResizePreview>, _, cx| {
                        // the content card ends 8 px (mr_2) before the row's right edge
                        // mr_2 (8 px) + the card's 1 px border
                        let w = f32::from(e.bounds.right() - e.event.position.x) - 9.;
                        this.set_pane_width(false, w, cx);
                    }))
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ResizeSidebar>, _, cx| {
                        let w = f32::from(e.event.position.x - e.bounds.left());
                        this.set_pane_width(true, w, cx);
                    }))
                    .children(sidebar.map(|s| {
                        div()
                            .id("sidebar-slot")
                            .h_full()
                            .relative()
                            .flex_shrink_0()
                            .child(s)
                            .child(resize_handle("sidebar-resize", ResizeSidebar, false))
                            // on the rail, hovering opens the full sidebar over the content
                            .on_hover(cx.listener(|this, h: &bool, window, cx| {
                                if this.sidebar_rail && this.rail_peek[0] != *h {
                                    if *h && !this.rail_peek[1] {
                                        // before the sidebar covers it: a column near the rail's
                                        // left edge, clear of the icons
                                        let s = window.scale_factor();
                                        let height = f32::from(window.viewport_size().height);
                                        let at = |v: f32| (v * s) as i32;
                                        this.rail_peek_bg = rf_win::window::screen_color(crate::look::window::hwnd_of(window), at(3.), at(110.), at(height - 60.))
                                            .map(|[r, g, b]| Hsla::from(gpui_kit::rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))));
                                    }
                                    this.rail_peek[0] = *h;
                                    cx.notify();
                                }
                            }))
                            // drawn after the list and the preview, so it lies over them
                            .children(peek.map(|d| deferred(d).with_priority(1)))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .mr_2()
                            .rounded(theme::RADIUS)
                            .bg(p.content_bg)
                            .border_1()
                            .border_color(p.border)
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .when(show_commandbar && show_toolbar, |d| d.child(commandbar))
                            .children(filter_strip)
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_h_0()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .h_full()
                                            .relative()
                                            // Drop a tab here: it moves into a new window.
                                            .drag_over::<TabDrag>(move |s, _, _, _| s.bg(p.selection))
                                            .on_drop(cx.listener(|this, d: &TabDrag, window, cx| this.detach_tab(d.ix, window, cx)))
                                            .child(content),
                                    )
                                    .children(shelf)
                                    .children(preview),
                            )
                            .children(star_strip)
                            .children(donate_strip)
                            .when(show_statusbar, |d| d.child(status))
                            .children(picker_footer),
                    ),
            )
            .child(div().h(px(8.)))
            // Last children = painted on top.
            .children(crate::status_center::render_flyout(&self.ops, &p, theme::TITLEBAR_H + px(44.), cx))
            .children(crate::status_center::update_toast(&p, self.picker.is_some()))
            .children(quick_look_layer)
            .children(self.palette.clone())
            .children(crate::fluent_menu::current(window, cx))
            .children(crate::prompt::current(window, cx))
            .children(crate::conflict_dialog::current(window, cx))
            .children(self.bulk.clone().map(|br| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(hsla(0., 0., 0., 0.3))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(br)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::{breadcrumbs, parse_location};
    use rf_core::Location;
    use std::path::{Path, PathBuf};

    #[test]
    fn crumbs() {
        let c = breadcrumbs(Path::new(r"C:\Users\demo"));
        let labels: Vec<_> = c.iter().map(|(l, _)| l.as_str()).collect();
        // C:\Users shows Explorer's localized name ("Kullanıcılar" on Turkish Windows)
        let users = rf_win::localized::folder_label(Path::new(r"C:\Users"), None).map_or("Users".to_string(), |l| l.to_string());
        assert_eq!(labels, vec!["C:", users.as_str(), "demo"]);
        assert_ne!(users, "");
        assert_eq!(c[0].1, Location::Path(PathBuf::from(r"C:\")));
        assert_eq!(parse_location("d:"), Some(Location::Path(PathBuf::from(r"d:\"))));
    }
}

#[cfg(test)]
mod launch_tests {
    use super::{SELECT_PREFIX, parse_launch};
    use rf_core::Location;
    use std::path::PathBuf;

    #[test]
    fn select_messages() {
        let (loc, sel) = parse_launch(&format!(r"{SELECT_PREFIX}C:\Klasör\rapor ü.txt"));
        assert_eq!(loc, Location::Path(PathBuf::from(r"C:\Klasör")));
        assert_eq!(sel.as_deref(), Some("rapor ü.txt"));
        assert_eq!(parse_launch(r"C:\Windows"), (Location::Path(PathBuf::from(r"C:\Windows")), None));
        assert_eq!(parse_launch("  "), (Location::Home, None));
    }

    #[test]
    fn session_tabs_come_back_as_they_were() {
        let phone = Location::Shell {
            parsing: r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\\?\usb#vid_04e8&pid_6860#r58n#{6ac27878-a6fa-4155-ba85-f98f491d4f33}".into(),
            name: "Galaxy S24".into(),
        };
        for loc in [Location::Path(PathBuf::from(r"C:\Kullanıcılar\Özer")), Location::RecycleBin, phone] {
            assert_eq!(parse_launch(&super::session_text(&loc)).0, loc);
        }
        // places a session can't keep come back as Home
        assert_eq!(super::session_text(&Location::Settings), "");
    }
}
