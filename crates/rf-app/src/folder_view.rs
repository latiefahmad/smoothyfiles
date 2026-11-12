//! One tab's content: a location, its listing, selection, and the details view.
//!
//! Listing runs on a dedicated thread and streams batches back; the UI thread only
//! merges batches and paints. Navigating away cancels the in-flight listing.

use crate::t;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui_kit::assets::IconName as L;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rf_core::entry::Filter;
use rf_core::{Entry, History, Location, SortDir, SortKey, SortSpec, format};

use crate::actions::*;
use crate::icons::{self, Category};
use crate::image_cache::ImageCache;
use crate::places::{self, Places};
use crate::theme::{self, Palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Details,
    Grid,
    /// Finder-style Miller columns.
    Columns,
}

/// Look of the grid view (Files: Grid, Cards, List).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridStyle {
    #[default]
    Tiles,
    /// Wide tiles with type and size next to the picture.
    Cards,
    /// Small icon + name, many per row.
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    None,
    Type,
    Modified,
    Size,
    Name,
}

/// A line of the details list when grouping is on.
#[derive(Clone)]
enum Row {
    Header(SharedString, usize),
    Item(usize),
}

fn group_key(g: GroupBy, e: &Entry, now: chrono::DateTime<chrono::Local>) -> (u32, String) {
    use chrono::{Datelike, TimeZone};
    match g {
        GroupBy::None => (0, String::new()),
        GroupBy::Type => {
            if e.is_dir() { (0, "Klasörler".into()) } else { (1, icons::type_label(e)) }
        }
        GroupBy::Modified => {
            let Some(t) = chrono::Local.timestamp_millis_opt(e.modified_ms).single() else {
                return (6, "Bilinmiyor".into());
            };
            let days = (now.date_naive() - t.date_naive()).num_days();
            match days {
                d if d <= 0 => (0, "Bugün".into()),
                1 => (1, "Dün".into()),
                d if d < 7 && t.iso_week() == now.iso_week() => (2, "Bu hafta".into()),
                _ if t.year() == now.year() && t.month() == now.month() => (3, "Bu ay".into()),
                _ if t.year() == now.year() => (4, "Bu yıl".into()),
                _ => (5, "Daha eski".into()),
            }
        }
        GroupBy::Size => {
            if e.is_dir() {
                return (0, "Klasörler".into());
            }
            match e.size {
                0 => (1, "Boş".into()),
                s if s < 16 * 1024 => (2, "Çok küçük (< 16 KB)".into()),
                s if s < 1024 * 1024 => (3, "Küçük (< 1 MB)".into()),
                s if s < 128 * 1024 * 1024 => (4, "Orta (< 128 MB)".into()),
                s if s < 1024 * 1024 * 1024 => (5, "Büyük (< 1 GB)".into()),
                _ => (6, "Çok büyük (≥ 1 GB)".into()),
            }
        }
        GroupBy::Name => {
            let c = e.name.chars().next().unwrap_or('#');
            if c.is_alphabetic() {
                let up: String = if c == 'i' { "İ".into() } else { c.to_uppercase().collect() };
                (1, up)
            } else {
                (0, "#".into())
            }
        }
    }
}

struct Marquee {
    start: Point<Pixels>,
    cur: Point<Pixels>,
    base: HashSet<u32>,
}

pub enum FolderEvent {
    OpenInNewTab(Location),
    BulkRename(Vec<PathBuf>),
    AddToShelf(Vec<PathBuf>),
    /// Dual pane: copy/move the items into the other pane's folder.
    ToOtherPane(rf_win::fileops::Transfer, Vec<PathBuf>),
    /// Dual pane: show a folder in the other pane.
    OpenInOtherPane(Location),
    OpenInNewWindow(Location),
    /// Space / Esc: open or close Quick Look for the focused item.
    QuickLook,
    /// Browsing into a protected archive: (archive, the given password was wrong).
    NeedPassword(PathBuf, bool),
    /// Open/Save dialog: files were opened (double click, Enter): they are the answer.
    Picked(Vec<PathBuf>),
}

/// Details columns shown right now (see `FolderView::visible_cols`).
#[derive(Clone, Copy)]
struct Cols {
    modified: bool,
    created: bool,
    kind: bool,
    size: bool,
    tags: bool,
    git: bool,
    sync: bool,
    /// commit message, author, date, hash
    commit: [bool; 4],
}

/// Drag payload of a details-header column edge.
#[derive(Clone)]
struct ColumnResize;
struct ColumnGhost;
impl Render for ColumnGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Quick Look is showing (Esc closes it instead of clearing the selection).
pub static QUICK_LOOK_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Filter + sort + grouping result, computable on any thread.
struct View {
    visible: Vec<u32>,
    /// Every entry in sort order (filter applied after): a filter change reuses it, no sort.
    sorted: Arc<Vec<u32>>,
    rows: Vec<Row>,
    row_of_vi: Vec<usize>,
    /// What it was computed with (discarded if the user changed any of these meanwhile).
    spec: (Filter, SortSpec, GroupBy),
    len: usize,
}

fn compute_view(entries: &[Entry], filter: &Filter, sort: SortSpec, group: GroupBy) -> View {
    let mut all: Vec<u32> = (0..entries.len() as u32).collect();
    sort.sort_indices(entries, &mut all);
    view_from_sorted(entries, Arc::new(all), filter, sort, group)
}

/// [`compute_view`] from entries already in sort order: filtering only (typing in the filter
/// box of a 100,000-file folder costs a pass over the names, not a sort per keystroke).
fn view_from_sorted(entries: &[Entry], sorted: Arc<Vec<u32>>, filter: &Filter, sort: SortSpec, group: GroupBy) -> View {
    let mut v: Vec<u32> = sorted.iter().copied().filter(|&i| filter.accepts(&entries[i as usize])).collect();
    let mut rows = Vec::new();
    let mut row_of_vi = Vec::new();
    if group != GroupBy::None {
        let now = chrono::Local::now();
        let keys: std::collections::HashMap<u32, (u32, String)> =
            v.iter().map(|&i| (i, group_key(group, &entries[i as usize], now))).collect();
        // stable: keeps the sort order inside each group
        v.sort_by(|a, b| keys[a].cmp(&keys[b]));
        let mut current: Option<&(u32, String)> = None;
        let mut header_ix = 0;
        row_of_vi = Vec::with_capacity(v.len());
        for (vi, i) in v.iter().enumerate() {
            let k = &keys[i];
            if current != Some(k) {
                current = Some(k);
                header_ix = rows.len();
                rows.push(Row::Header(k.1.clone().into(), 0));
            }
            if let Row::Header(_, n) = &mut rows[header_ix] {
                *n += 1;
            }
            row_of_vi.push(rows.len());
            rows.push(Row::Item(vi));
        }
    }
    View { visible: v, sorted, rows, row_of_vi, spec: (filter.clone(), sort, group), len: entries.len() }
}

enum Msg {
    Batch(Vec<Entry>),
    /// Final sorted view computed on the listing thread (keeps big sorts off the UI thread).
    View(View),
    /// Search hits: entries plus their parent folders.
    Hits(Vec<Entry>, Vec<Arc<std::path::Path>>),
    /// Shell folder items (a phone, Network…): entries plus what the shell knows about each.
    Shell(Vec<Entry>, Vec<rf_win::shellfolder::ShellChild>),
    /// The shell's own name for the folder being listed.
    ShellName(String),
    Done(Result<usize, String>),
}

/// A folder's size in the Size column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirSize {
    /// Being measured: bytes counted so far.
    Partial(u64),
    Done(rf_win::foldersize::FolderSize),
}

impl From<rf_win::foldersize::SizeState> for DirSize {
    fn from(s: rf_win::foldersize::SizeState) -> Self {
        match s {
            rf_win::foldersize::SizeState::Partial(b) => DirSize::Partial(b),
            // a size remembered from the last run stands in until the new measurement ends; the
            // fresh value then replaces it without any marking
            rf_win::foldersize::SizeState::Provisional(s) | rf_win::foldersize::SizeState::Done(s) => DirSize::Done(s),
        }
    }
}

impl DirSize {
    /// "…" until something is counted, then the running total with "…", then the size.
    pub(crate) fn text(self, on_disk: bool) -> String {
        match self {
            DirSize::Partial(0) => "…".into(),
            DirSize::Partial(b) => format!("{}…", format::size(b)),
            DirSize::Done(s) => format::size(Self::bytes(s, on_disk)),
        }
    }

    /// The size to show and sort by; without a known cluster size the logical size stands in.
    fn bytes(s: rf_win::foldersize::FolderSize, on_disk: bool) -> u64 {
        if on_disk { s.allocated.unwrap_or(s.bytes) } else { s.bytes }
    }
}

pub struct FolderView {
    pub history: History,
    /// Set by the workspace while this view is one of two panes.
    pub(crate) dual: bool,
    /// (library file, default save folder) of the library being shown (paste/new item target).
    pub(crate) library_save: Option<(PathBuf, PathBuf)>,
    places: Entity<Places>,
    pub(crate) images: Entity<ImageCache>,
    pub view_mode: ViewMode,
    grid_cols: usize,
    /// Index into GRID_SIZES.
    grid_size: u8,
    /// Index into ROW_SIZES (Details row height), the "Size" slider in Details.
    row_size: u8,
    /// Column resize in progress: (column, mouse x at start, width at start), live widths.
    col_drag: Option<(usize, f32, f32)>,
    col_live: Option<[f32; 11]>,
    col_save_gen: u64,
    pub(crate) grid_style: GridStyle,
    content_width: Pixels,
    pub(crate) entries: Vec<Entry>,
    /// Search mode only: parent folder of each entry (aligned with `entries`).
    parents: Vec<Arc<std::path::Path>>,
    /// Shell folders only: what the shell knows about each entry (aligned with `entries`).
    pub(crate) shell_items: Vec<rf_win::shellfolder::ShellChild>,
    /// Shell folders only: the shell's name for the folder (when the location carries none).
    shell_title: Option<String>,
    visible: Vec<u32>,
    /// The entries in sort order, for the sort and the entries generation it was made with.
    sorted_cache: Option<(SortSpec, u64, Arc<Vec<u32>>)>,
    /// Changes whenever `entries` do (cache key of `sorted_cache`).
    entries_gen: u64,
    /// Whether any entry is a cloud file, for the entries generation it was worked out for (the
    /// Sync column; asked several times per frame).
    cloud_cache: std::cell::Cell<Option<(u64, bool)>>,
    pub sort: SortSpec,
    pub group: GroupBy,
    /// Details rows (headers + items) when grouping; empty = identity with `visible`.
    rows: Vec<Row>,
    /// visible index → row index (grouping only).
    row_of_vi: Vec<usize>,
    pub filter: Filter,
    pub(crate) selected: HashSet<u32>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    pub loading: bool,
    pub error: Option<String>,
    pub load_ms: Option<u128>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    /// Bumped on every navigation; a late watcher for an old folder is discarded.
    watch_gen: u64,
    load_started: Instant,
    _load_task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    pub focus: FocusHandle,
    /// Re-select this name after the next load (e.g. after going up a level).
    reselect: Option<String>,
    /// Start inline rename on `reselect` once it appears (new folder flow).
    rename_after_load: bool,
    /// Items that just arrived (a paste): selected as they show up in the list, until all are
    /// there or the deadline passes (the watcher can lag behind the copy).
    reselect_many: Option<(Vec<String>, Instant)>,
    /// Begin inline rename on the next render (needs a Window).
    pending_rename: bool,
    /// Shown by the Open/Save dialog: opening a file picks it instead.
    picking: bool,
    /// The mouse went down on an item's name (set by the name label, read by the row).
    name_hit: bool,
    /// Explorer's "click the name of the selected item again": (entry, serial) armed until the
    /// double-click time passes without a second click or a drag.
    slow_click: Option<(u32, u64)>,
    slow_click_serial: u64,
    /// Entries of an in-flight background refresh (swapped in atomically when complete).
    soft_pending: Option<Vec<Entry>>,
    /// When Space opened Quick Look (a long press closes it again on release).
    space_hold: Option<Instant>,
    /// When the current soft reload (watcher) started (for the live trace).
    soft_started: Instant,
    _watch: Option<rf_win::watch::DirWatch>,
    _watch_task: Option<Task<()>>,
    pub renaming: Option<(u32, Entity<gpui_kit::component::input::InputState>)>,
    recycle: Option<Entity<crate::recycle_view::RecycleView>>,
    /// The disk usage page of this tab (kept while the tab stays on that folder).
    disk_usage: Option<Entity<crate::disk_usage::DiskUsageView>>,
    /// Git status of the repository containing this folder (if any).
    pub git: Option<Arc<rf_win::git::GitStatus>>,
    /// Last commit per child of the current folder (commit columns), keyed by name.
    git_commits: Option<Arc<std::collections::HashMap<String, rf_win::git::Commit>>>,
    /// Columns view: listings of ancestor / next folders (sorted, filtered).
    column_cache: std::collections::HashMap<PathBuf, Arc<Vec<Entry>>>,
    column_pending: HashSet<PathBuf>,
    /// Folder sizes for the Size column: clicked ("Boyutu hesapla") or all visible ones
    /// (Settings → "Calculate folder sizes").
    folder_sizes: std::collections::HashMap<PathBuf, DirSize>,
    /// Settings → Layout → "Adaptive": decide details/grid once this folder's listing is in.
    adaptive_pending: bool,
    /// Requested this frame; started together as one batch.
    size_queue: Vec<PathBuf>,
    size_batches: Vec<(Arc<rf_win::foldersize::SizeBatch>, Vec<PathBuf>)>,
    size_polling: bool,
    col_scroll: ScrollHandle,
    col_key: Option<(PathBuf, Option<PathBuf>)>,
    /// Per-ancestor-column scroll handles, and which (dir, highlighted child) we already scrolled to.
    col_scrolls: std::collections::HashMap<PathBuf, UniformListScrollHandle>,
    col_scrolled: HashSet<(PathBuf, String)>,
    _git_task: Option<Task<()>>,
    /// When git status was last asked for, and a later ask waiting (changes coming in fast).
    git_last: Option<Instant>,
    git_soon: Option<Task<()>>,
    /// Re-renders once a load has been running for a while (shows the "loading" placeholder).
    _slow_task: Option<Task<()>>,
    /// Where a left-button press started on a row (for drag-out detection).
    drag_origin: Option<Point<Pixels>>,
    /// Rubber-band selection in list-content coordinates.
    marquee: Option<Marquee>,
    /// Type-to-select buffer and when it was last extended.
    typeahead: (String, Instant),
    _rename_sub: Option<Subscription>,
}

impl EventEmitter<FolderEvent> for FolderView {}

impl Focusable for FolderView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FolderView {
    pub fn new(location: Location, places: Entity<Places>, images: Entity<ImageCache>, cx: &mut Context<Self>) -> Self {
        cx.observe(&places, |_, _, cx| cx.notify()).detach();
        cx.observe(&images, |_, _, cx| cx.notify()).detach();
        cx.observe_global::<crate::settings::Settings>(|this, cx| {
            let s = cx.global::<crate::settings::Settings>();
            let (hidden, system, dot) = (s.show_hidden, s.show_system_files, !s.show_dot_files);
            if (this.filter.show_hidden, this.filter.show_system, this.filter.dot_hidden) != (hidden, system, dot) {
                this.filter.show_hidden = hidden;
                this.filter.show_system = system;
                this.filter.dot_hidden = dot;
                this.rebuild_visible();
            }
            // folders show (and sort by) size on disk or the sum of their files, as set
            this.resort_for_sizes(cx);
            // turning a commit column on loads the commits
            if this.git.is_some() && this.git_commits.is_none() && Self::commit_cols_on(cx) {
                this.refresh_git(cx);
            }
            cx.notify();
        })
        .detach();
        let prefs = cx.global::<crate::settings::Settings>().clone();
        let mut this = Self {
            history: History::new(location),
            dual: false,
            library_save: None,
            places,
            images,
            view_mode: match prefs.default_view {
                // Adaptive starts in details and switches once the listing shows a media folder
                crate::settings::DefaultView::Details | crate::settings::DefaultView::Adaptive => ViewMode::Details,
                crate::settings::DefaultView::Grid => ViewMode::Grid,
                crate::settings::DefaultView::Columns => ViewMode::Columns,
            },
            grid_cols: 1,
            grid_size: DEFAULT_GRID_SIZE,
            row_size: DEFAULT_ROW_SIZE,
            col_drag: None,
            col_live: None,
            col_save_gen: 0,
            grid_style: GridStyle::Tiles,
            content_width: px(900.),
            entries: Vec::new(),
            sorted_cache: None,
            entries_gen: 0,
            cloud_cache: Default::default(),
            parents: Vec::new(),
            shell_items: Vec::new(),
            shell_title: None,
            visible: Vec::new(),
            sort: SortSpec {
                key: prefs.default_sort,
                dir: if prefs.default_sort_desc { SortDir::Desc } else { SortDir::Asc },
                dirs_first: prefs.folders_first,
                files_first: !prefs.folders_first && prefs.files_first,
            },
            group: GroupBy::None,
            rows: Vec::new(),
            row_of_vi: Vec::new(),
            filter: Filter {
                show_hidden: prefs.show_hidden,
                show_system: prefs.show_system_files,
                dot_hidden: !prefs.show_dot_files,
                ..Filter::default()
            },
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            loading: false,
            error: None,
            load_ms: None,
            cancel: Arc::new(AtomicBool::new(false)),
            generation: 0,
            watch_gen: 0,
            load_started: Instant::now(),
            _load_task: None,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            reselect: None,
            rename_after_load: false,
            reselect_many: None,
            pending_rename: false,
            picking: false,
            name_hit: false,
            slow_click: None,
            slow_click_serial: 0,
            soft_pending: None,
            space_hold: None,
            soft_started: Instant::now(),
            _watch: None,
            _watch_task: None,
            renaming: None,
            _rename_sub: None,
            drag_origin: None,
            marquee: None,
            typeahead: (String::new(), Instant::now()),
            recycle: None,
            disk_usage: None,
            git: None,
            git_commits: None,
            column_cache: Default::default(),
            column_pending: HashSet::new(),
            folder_sizes: Default::default(),
            adaptive_pending: false,
            size_queue: Default::default(),
            size_batches: Default::default(),
            size_polling: false,
            col_scroll: ScrollHandle::new(),
            col_key: None,
            col_scrolls: Default::default(),
            col_scrolled: HashSet::new(),
            _git_task: None,
            git_last: None,
            git_soon: None,
            _slow_task: None,
        };
        this.load(cx);
        this
    }

    pub fn location(&self) -> &Location {
        self.history.current()
    }

    pub fn title(&self, cx: &App) -> String {
        if let Location::Tag(id) = self.location() {
            if let Some(d) = cx.global::<crate::tags::Tags>().0.def(*id) {
                return format!("{} {}", crate::t!("Etiket:"), crate::i18n::td(&d.name));
            }
        }
        if let Location::Shell { name, .. } = self.location() {
            if name.is_empty() {
                if let Some(t) = &self.shell_title {
                    return t.clone();
                }
            }
        }
        if let Location::Path(p) = self.location() {
            if let Some(l) = rf_win::localized::folder_label(p, None) {
                return l.to_string();
            }
        }
        if let Location::DiskUsage(p) = self.location() {
            return crate::disk_usage::title(p);
        }
        crate::i18n::td(&self.location().title())
    }

    pub fn tab_icon(&self) -> L {
        match self.location() {
            Location::Home => L::House,
            Location::Settings => L::Settings,
            Location::RecycleBin => L::Trash,
            Location::Search { .. } => L::Search,
            Location::Tag(_) => L::Tag,
            Location::Library { .. } => L::Library,
            Location::Shell { .. } => L::Usb,
            Location::DiskUsage(_) => L::ChartPie,
            Location::Path(p) if rf_win::network::server_root(p).is_some() => L::Server,
            // the archive itself or a folder inside it (name check only: no disk access here)
            Location::Path(p) if crate::archive_view::is_browsable_path(p) => L::FileArchive,
            Location::Path(p) => {
                if p.parent().is_none() {
                    L::HardDrive
                } else {
                    L::Folder
                }
            }
        }
    }

    // ---------- navigation ----------

    pub fn navigate(&mut self, to: Location, cx: &mut Context<Self>) {
        if to == Location::RecycleBin {
            self.recycle = None; // fresh listing each visit
        }
        self.history.navigate(to);
        self.filter.query.clear();
        self.load(cx);
    }

    pub fn back(&mut self, cx: &mut Context<Self>) {
        if self.history.back().is_some() {
            self.load(cx);
        }
    }

    pub fn forward(&mut self, cx: &mut Context<Self>) {
        if self.history.forward().is_some() {
            self.load(cx);
        }
    }

    /// Jumps `n` steps through history (from the back/forward button menus).
    pub fn jump(&mut self, back: bool, n: usize, cx: &mut Context<Self>) {
        let moved = if back { self.history.back_n(n) } else { self.history.forward_n(n) };
        if moved {
            self.load(cx);
        }
    }

    pub fn up(&mut self, cx: &mut Context<Self>) {
        let cur = self.location().clone();
        if let Some(parent) = cur.parent() {
            // Settings → "Select the previous folder when going up"
            if cx.global::<crate::settings::Settings>().select_previous_on_up {
                self.reselect = cur.path().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
            }
            self.navigate(parent, cx);
        }
    }

    fn ensure_column(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.column_cache.contains_key(&dir) || !self.column_pending.insert(dir.clone()) {
            return;
        }
        if self.column_cache.len() > 48 {
            self.column_cache.clear();
        }
        let show_hidden = self.filter.show_hidden;
        let d2 = dir.clone();
        let work = cx.background_spawn(async move {
            let mut v = Vec::new();
            let _ = rf_win::list_dir(&d2, 4096, &|| false, &mut |b| v.extend(b));
            v.retain(|e| show_hidden || !e.is_hidden());
            let spec = SortSpec::default();
            v.sort_by(|a, b| spec.compare(a, b));
            v
        });
        cx.spawn(async move |this, cx| {
            let v = work.await;
            let _ = this.update(cx, |t, cx| {
                t.column_pending.remove(&dir);
                t.column_cache.insert(dir, Arc::new(v));
                cx.notify();
            });
        })
        .detach();
    }

    fn render_col_rows(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = Palette::for_window(window);
        let mut out = Vec::with_capacity(range.len());
        for vi in range {
            let Some(&ei) = self.visible.get(vi) else { break };
            let e = &self.entries[ei as usize];
            let selected = self.selected.contains(&ei);
            let cat = icons::category(e);
            let shell_icon = self.parent_of(ei).map(|d| d.to_path_buf()).and_then(|d| self.images.update(cx, |c, cx| c.icon(&d, e, cx)));
            let is_dir = e.is_dir();
            out.push(
                h_flex()
                    .id(("crow", vi))
                    .w_full()
                    .h(px(28.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .relative()
                    .when(selected, |d| d.bg(p.selection).child(theme::selection_pill(&p)))
                    .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
                    .child(match &shell_icon {
                        Some(im) => img(im.clone()).size(px(16.)).flex_shrink_0().into_any_element(),
                        None => Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, p.dark)).into_any_element(),
                    })
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.name_hit = true)).child(SharedString::from(e.name.to_string())))
                    .when(is_dir, |d| d.child(Icon::new(L::ChevronRight).size(px(12.)).text_color(p.text_tertiary)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| this.on_row_mouse_down(vi, ev, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| this.on_row_right_down(vi, window, cx)),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn render_columns(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(cur) = self.current_dir() else { return div().into_any_element() };
        // Up to two ancestor columns, each highlighting the child on the current path.
        let mut ancestors: Vec<(PathBuf, String)> = Vec::new();
        let mut child = cur.clone();
        for _ in 0..2 {
            let Some(parent) = child.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf()) else { break };
            ancestors.push((parent.clone(), rf_core::history::display_name(&child)));
            child = parent;
        }
        ancestors.reverse();
        let next_dir = self.cursor_entry().filter(|&ei| self.entries[ei as usize].is_dir()).and_then(|ei| self.path_of(ei));
        for (d, _) in &ancestors {
            self.ensure_column(d.clone(), cx);
        }
        if let Some(n) = &next_dir {
            self.ensure_column(n.clone(), cx);
        }
        let key = (cur.clone(), next_dir.clone());
        if self.col_key.as_ref() != Some(&key) {
            self.col_key = Some(key);
            let n_cols = ancestors.len() + 1 + usize::from(next_dir.is_some());
            self.col_scroll.scroll_to_item(n_cols.saturating_sub(1));
        }

        let entity = cx.entity();
        let mut strip = h_flex().id("columns").size_full().overflow_x_scroll().track_scroll(&self.col_scroll);
        for (k, (dir, on_path)) in ancestors.into_iter().enumerate() {
            let list = self.column_cache.get(&dir).cloned();
            let handle = self.col_scrolls.entry(dir.clone()).or_insert_with(UniformListScrollHandle::new).clone();
            // Bring the highlighted child into view once its listing is loaded.
            if let Some(l) = &list {
                if self.col_scrolled.insert((dir.clone(), on_path.clone())) {
                    if let Some(i) = l.iter().position(|e| *e.name == *on_path) {
                        handle.scroll_to_item(i, ScrollStrategy::Center);
                    }
                }
            }
            strip = strip.child(static_column(("col-anc", k).into(), dir, list, Some(on_path), Some(handle), p, entity.clone()));
        }
        let count = self.visible.len();
        strip = strip.child(
            div()
                .w(COL_W)
                .flex_shrink_0()
                .h_full()
                .border_r_1()
                .border_color(p.divider)
                .child(
                    uniform_list("col-cur", count, cx.processor(|this, range, window, cx| this.render_col_rows(range, window, cx)))
                        .track_scroll(&self.scroll)
                        .size_full()
                        .p_1(),
                ),
        );
        if let Some(nd) = next_dir {
            let list = self.column_cache.get(&nd).cloned();
            strip = strip.child(static_column("col-next".into(), nd, list, None, None, p, entity));
        }
        strip.into_any_element()
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.column_cache.clear();
        // F5 measures folder sizes afresh (deep changes don't move a folder's own date)
        if let Some(p) = self.location().path() {
            rf_win::foldersize::shared_cache().invalidate(p);
        }
        if matches!(self.location(), Location::Home) {
            self.places.update(cx, |p, cx| p.refresh(cx));
        }
        if let (Location::RecycleBin, Some(rv)) = (self.location(), self.recycle.clone()) {
            rv.update(cx, |r, cx| r.reload(cx));
        }
        // folders whose size was asked for are measured again, not forgotten
        let measured: Vec<PathBuf> = self.folder_sizes.keys().cloned().collect();
        self.load(cx);
        for p in measured {
            self.folder_size(p, cx);
        }
    }

    /// Re-reads git status for the current folder in the background (no-op outside a repo).
    /// [`Self::refresh_git`] for changes on the disk: at most every 2 s (each one runs `git
    /// status`, a process; a folder being written to would start one per change), the last
    /// change always followed by one.
    fn refresh_git_soon(&mut self, cx: &mut Context<Self>) {
        const GAP: std::time::Duration = std::time::Duration::from_secs(2);
        if self.git_soon.is_some() {
            return; // one is already waiting
        }
        let wait = self.git_last.map_or(std::time::Duration::ZERO, |t| GAP.saturating_sub(t.elapsed()));
        if wait.is_zero() {
            self.refresh_git(cx);
            return;
        }
        self.git_soon = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |v, cx| {
                v.git_soon = None;
                if v.git.is_some() {
                    v.refresh_git(cx);
                }
            });
        }));
    }

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.git_last = Some(Instant::now());
        let Some(dir) = self.location().path().map(|p| p.to_path_buf()).filter(|_| !self.is_search()) else {
            self.git = None;
            return;
        };
        let commits = Self::commit_cols_on(cx);
        let d = dir.clone();
        let work = cx.background_spawn(async move {
            let root = rf_win::git::repo_root(&d)?;
            Some((rf_win::git::status(&root).ok().map(Arc::new)?, root))
        });
        self._git_task = Some(cx.spawn(async move |this, cx| {
            let got = work.await;
            let Ok(Some(log)) = this.update(cx, |v, cx| {
                let (st, root) = got.map_or((None, None), |(s, r)| (Some(s), Some(r)));
                v.git = st.clone();
                cx.notify();
                // the status shows first; the commit columns follow from a slower `git log`
                let (st, root) = (st?, root?);
                commits.then(|| {
                    cx.background_spawn(async move {
                        // untracked / ignored children have no commit: don't scan history for them
                        let names: Vec<String> = std::fs::read_dir(&dir)
                            .map(|rd| {
                                rd.flatten()
                                    .map(|e| e.file_name().to_string_lossy().into_owned())
                                    .filter(|n| n != ".git")
                                    .filter(|n| {
                                        !matches!(
                                            rf_win::git::state_for_child(&st, &dir, n),
                                            Some(rf_win::git::FileState::Untracked | rf_win::git::FileState::Ignored)
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Arc::new(rf_win::git::last_commits_of_children(&root, &dir, &names, 5_000))
                    })
                })
            }) else {
                return;
            };
            let log = log.await;
            let _ = this.update(cx, |v, cx| {
                v.git_commits = Some(log);
                cx.notify();
            });
        }));
    }

    fn commit_cols_on(cx: &App) -> bool {
        let c = &cx.global::<crate::settings::Settings>().columns;
        c.git_message || c.git_author || c.git_date || c.git_hash
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.apply_folder_layout(cx);
        self.images.update(cx, |c, _| c.cancel_pending());
        self.git = None;
        self.git_commits = None;
        self.refresh_git(cx);
        self.renaming = None;
        self._rename_sub = None;
        self.load_inner(false, cx);
        self.start_watch(cx);
    }

    /// Re-lists the current folder in the background and swaps the result in, keeping
    /// selection and scroll position (used by the folder watcher and after file operations).
    pub fn soft_reload(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.location().path().is_none() || self.is_search() {
            return;
        }
        if self.git.is_some() {
            self.refresh_git_soon(cx);
        }
        self.load_inner(true, cx);
    }

    fn start_watch(&mut self, cx: &mut Context<Self>) {
        self._watch = None;
        self._watch_task = None;
        if self.is_search() {
            return;
        }
        let Some(path) = self.location().path().map(|p| p.to_path_buf()) else { return };
        if rf_win::ftp::is_ftp(&path) {
            return; // no change notifications over FTP; F5 refreshes
        }
        let (tx, mut rx) = mpsc::unbounded::<Option<Vec<(String, Option<Entry>)>>>();
        // a plain folder is patched with what changed; inside an archive it is listed again
        let patchable = crate::archive_view::split(&path).is_none();
        // Opening the directory handle can block (offline network share): do it off the UI thread.
        let setup = cx.background_spawn(async move {
            let dir = path.clone();
            rf_win::watch::watch_dir_changes(&path, std::time::Duration::from_millis(150), move |ch| {
                // the changed names read from the disk here, on the watcher's own thread (in
                // order, off the UI thread); None: list the whole folder again
                let patch = (patchable && !ch.rescan && !ch.names.is_empty())
                    .then(|| ch.names.into_iter().map(|n| rf_win::stat_entry(&dir, &n).map(|e| (n, e))).collect::<Result<Vec<_>, _>>().ok())
                    .flatten();
                let _ = tx.unbounded_send(patch);
            })
            .map_err(|e| log::warn!("watch {path:?}: {e}"))
            .ok()
        });
        self.watch_gen += 1;
        let generation = self.watch_gen;
        self._watch_task = Some(cx.spawn(async move |this, cx| {
            let Some(w) = setup.await else { return };
            let keep = this
                .update(cx, |v, _| {
                    if v.watch_gen == generation {
                        v._watch = Some(w);
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !keep {
                return;
            }
            while let Some(patch) = rx.next().await {
                let r = this.update(cx, |v, cx| match patch {
                    Some(p) if !v.loading && v.soft_pending.is_none() && !v.is_search() => v.apply_patch(p, cx),
                    _ => v.soft_reload(cx),
                });
                if r.is_err() {
                    break;
                }
            }
        }));
    }

    pub(crate) fn selected_is_single_dir(&self) -> bool {
        self.selected.len() == 1 && self.selected.iter().all(|&i| self.entries[i as usize].is_dir())
    }

    fn load_inner(&mut self, soft: bool, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        self._load_task = None;
        // a soft reload (watcher) keeps sizes and running walks; a real load starts over
        if !soft {
            self.folder_sizes.clear();
            self.size_queue.clear();
            for (b, _) in self.size_batches.drain(..) {
                b.cancel();
            }
        }
        if soft {
            self.soft_pending = Some(Vec::new());
            self.soft_started = Instant::now();
        } else {
            self.soft_pending = None;
            self.entries.clear();
            self.entries_gen += 1;
            self.parents.clear();
            self.shell_items.clear();
            self.shell_title = None;
            self.visible.clear();
            self.selected.clear();
            self.cursor = None;
            self.anchor = None;
            self.error = None;
            self.load_ms = None;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            // slow sources (offline share, FTP, big network folder): show a placeholder after a
            // moment instead of a blank pane; fast folders never see it
            self._slow_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SLOW_LOAD).await;
                let _ = this.update(cx, |v, cx| {
                    if v.loading {
                        cx.notify();
                    }
                });
            }));
        }

        if let Location::Library { file, .. } = self.location().clone() {
            self.library_save = self
                .places
                .read(cx)
                .libraries
                .iter()
                .find(|l| l.file == file)
                .and_then(|l| l.default_save.clone().or_else(|| l.folders.first().cloned()))
                .map(|d| (file.clone(), d));
            self.loading = true;
            self.load_started = Instant::now();
            let generation = self.generation;
            let cancel = self.cancel.clone();
            let (tx, mut rx) = mpsc::unbounded::<Msg>();
            std::thread::Builder::new()
                .name("rf-library".into())
                .spawn(move || {
                    let Some(lib) = rf_win::libraries::load(&file) else {
                        let _ = tx.unbounded_send(Msg::Done(Err("Kütüphane açılamadı".into())));
                        return;
                    };
                    let mut total = 0usize;
                    let mut last_err = None;
                    for dir in &lib.folders {
                        let parent: Arc<std::path::Path> = dir.as_path().into();
                        let r = rf_win::list_dir(dir, 512, &|| cancel.load(Ordering::Relaxed), &mut |b| {
                            let ps = vec![parent.clone(); b.len()];
                            let _ = tx.unbounded_send(Msg::Hits(b, ps));
                        });
                        match r {
                            Ok(n) => total += n,
                            Err(e) => last_err = Some(e.to_string()),
                        }
                    }
                    let res = if total == 0 { last_err.map_or(Ok(0), Err) } else { Ok(total) };
                    let _ = tx.unbounded_send(Msg::Done(res));
                })
                .ok();
            self._load_task = Some(cx.spawn(async move |this, cx| {
                let mut msgs = Vec::new();
                while let Some(m) = rx.next().await {
                    msgs.push(m);
                }
                let _ = this.update(cx, |v, cx| v.apply(generation, msgs, cx));
            }));
            cx.notify();
            return;
        }
        if let Location::Shell { parsing, .. } = self.location().clone() {
            self.loading = true;
            self.load_started = Instant::now();
            let generation = self.generation;
            let cancel = self.cancel.clone();
            let (tx, mut rx) = mpsc::unbounded::<Msg>();
            std::thread::Builder::new()
                .name("rf-shell-list".into())
                .spawn(move || {
                    use rf_win::shellfolder as sf;
                    if let Some(n) = sf::display_name(&parsing) {
                        let _ = tx.unbounded_send(Msg::ShellName(n));
                    }
                    let r = sf::list(&parsing, &|| cancel.load(Ordering::Relaxed), &mut |batch| {
                        let es = batch
                            .iter()
                            .map(|c| {
                                let kind = if c.is_folder { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File };
                                Entry::new(&c.name, kind, c.size.unwrap_or(0), c.modified_ms.unwrap_or(0), 0, 0)
                            })
                            .collect();
                        let _ = tx.unbounded_send(Msg::Shell(es, batch));
                    });
                    let _ = tx.unbounded_send(Msg::Done(r.map_err(|e| format!("{e:#}"))));
                })
                .ok();
            self._load_task = Some(cx.spawn(async move |this, cx| {
                while let Some(first) = rx.next().await {
                    let mut msgs = vec![first];
                    while let Ok(m) = rx.try_recv() {
                        msgs.push(m);
                    }
                    if this.update(cx, |v, cx| v.apply(generation, msgs, cx)).is_err() {
                        break;
                    }
                }
            }));
            cx.notify();
            return;
        }
        if let Location::Tag(id) = self.location().clone() {
            let paths = cx.global::<crate::tags::Tags>().0.paths_with(id);
            self.loading = true;
            self.load_started = Instant::now();
            let generation = self.generation;
            let (tx, mut rx) = mpsc::unbounded::<Msg>();
            std::thread::spawn(move || {
                use std::os::windows::fs::MetadataExt;
                let mut es = Vec::new();
                let mut ps: Vec<Arc<std::path::Path>> = Vec::new();
                for p in paths {
                    let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let kind = if m.is_dir() { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File };
                    es.push(Entry::new(
                        &name,
                        kind,
                        m.file_size(),
                        format::filetime_to_unix_ms(m.last_write_time()),
                        format::filetime_to_unix_ms(m.creation_time()),
                        m.file_attributes(),
                    ));
                    ps.push(p.parent().unwrap_or(&p).into());
                }
                let n = es.len();
                let _ = tx.unbounded_send(Msg::Hits(es, ps));
                let _ = tx.unbounded_send(Msg::Done(Ok(n)));
            });
            self._load_task = Some(cx.spawn(async move |this, cx| {
                let mut msgs = Vec::new();
                while let Some(m) = rx.next().await {
                    msgs.push(m);
                }
                let _ = this.update(cx, |v, cx| v.apply(generation, msgs, cx));
            }));
            cx.notify();
            return;
        }
        let Some(path) = self.location().path().map(|p| p.to_path_buf()) else {
            self.loading = false;
            cx.notify();
            return;
        };
        if !soft {
            self.loading = true;
            self.load_started = Instant::now();
        }
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let (tx, mut rx) = mpsc::unbounded::<Msg>();
        if rf_win::ftp::is_ftp(&path) {
            let url = path.to_string_lossy().into_owned();
            std::thread::Builder::new()
                .name("rf-ftp".into())
                .spawn(move || {
                    let r = rf_win::ftp::list(&url).map(|es| {
                        let n = es.len();
                        let _ = tx.unbounded_send(Msg::Batch(es));
                        n
                    });
                    let _ = tx.unbounded_send(Msg::Done(r.map_err(|e| e.to_string())));
                })
                .ok();
        } else if let Some(server) = rf_win::network::server_root(&path) {
            std::thread::Builder::new()
                .name("rf-shares".into())
                .spawn(move || {
                    let r = rf_win::network::shares(&server).map(|list| {
                        let es: Vec<Entry> = list
                            .iter()
                            .map(|s| Entry::new(&s.name, rf_core::EntryKind::Dir, 0, 0, 0, 0x10))
                            .collect();
                        let n = es.len();
                        let _ = tx.unbounded_send(Msg::Batch(es));
                        n
                    });
                    let _ = tx.unbounded_send(Msg::Done(r.map_err(|e| e.to_string())));
                })
                .ok();
        } else if let Location::Search { root, query } = self.location().clone() {
            let include_hidden = self.filter.show_hidden;
            let use_index = cx.global::<crate::settings::Settings>().search_index;
            // "tag:Kırmızı" / "etiket:kırmızı": tagged items under this folder (Files' tag: query)
            let tag_paths: Option<Vec<PathBuf>> = {
                let lower = rf_core::entry::fold_case(&query);
                lower.strip_prefix("tag:").or_else(|| lower.strip_prefix("etiket:")).map(|name| {
                    let name = name.trim().to_string();
                    let store = &cx.global::<crate::tags::Tags>().0;
                    store
                        .defs
                        .iter()
                        .filter(|d| rf_core::entry::fold_case(&d.name) == name || rf_core::entry::fold_case(&crate::i18n::td(&d.name)) == name)
                        .flat_map(|d| store.paths_with(d.id))
                        .filter(|p| p.starts_with(&root))
                        .collect()
                })
            };
            std::thread::Builder::new()
                .name("rf-search".into())
                .spawn(move || {
                    if let Some(paths) = tag_paths {
                        let mut es = Vec::new();
                        let mut ps: Vec<Arc<std::path::Path>> = Vec::new();
                        for p in paths {
                            use std::os::windows::fs::MetadataExt;
                            let Ok(m) = std::fs::metadata(&p) else { continue };
                            let ms = |t: std::io::Result<std::time::SystemTime>| {
                                t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
                            };
                            let kind = if m.is_dir() { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File };
                            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            es.push(Entry::new(&name, kind, if m.is_dir() { 0 } else { m.len() }, ms(m.modified()), ms(m.created()), m.file_attributes()));
                            ps.push(p.parent().unwrap_or(&p).into());
                        }
                        let n = es.len();
                        let _ = tx.unbounded_send(Msg::Hits(es, ps));
                        let _ = tx.unbounded_send(Msg::Done(Ok(n)));
                        return;
                    }
                    // 1. our name index (milliseconds) when it covers this folder
                    let glob = query.contains(['*', '?']);
                    let mut seen: Option<std::collections::HashSet<String>> = None;
                    let mut found = 0usize;
                    let t_index = std::time::Instant::now();
                    if let Some(paths) = rf_win::indexer::search(&query, &root, include_hidden, 50_000) {
                        let looked_up = t_index.elapsed();
                        found = send_index_hits(&paths, &tx, &cancel);
                        log::info!("search: the index answered \"{query}\" in {} with {} hits ({looked_up:?} to find, {:?} with details)", root.display(), found, t_index.elapsed());
                        seen = Some(paths.iter().map(|p| p.to_string_lossy().to_lowercase()).collect());
                    }
                    // 2. Windows Search (names + contents + AQS) where the folder is indexed: with
                    //    our index, only what it adds (matches inside files)
                    if use_index && !glob && rf_win::wsearch::is_indexed(&root) {
                        let tx2 = tx.clone();
                        let seen2 = seen.as_ref();
                        let r = rf_win::wsearch::search(&root, &query, 50_000, &|| cancel.load(Ordering::Relaxed), &mut |hits| {
                            let mut es = Vec::with_capacity(hits.len());
                            let mut ps = Vec::with_capacity(hits.len());
                            for h in hits {
                                if !include_hidden && h.entry.is_hidden() {
                                    continue;
                                }
                                if seen2.is_some_and(|s| s.contains(&h.path.to_string_lossy().to_lowercase())) {
                                    continue;
                                }
                                let parent: Arc<std::path::Path> = h.path.parent().unwrap_or(&h.path).into();
                                ps.push(parent);
                                es.push(h.entry);
                            }
                            let _ = tx2.unbounded_send(Msg::Hits(es, ps));
                        });
                        match r {
                            Ok(n) => {
                                let _ = tx.unbounded_send(Msg::Done(Ok(n + found)));
                                return;
                            }
                            Err(e) if seen.is_none() => log::warn!("index search failed, walking instead: {e:#}"),
                            Err(e) => log::warn!("Windows Search failed: {e:#}"),
                        }
                    }
                    if seen.is_some() {
                        let _ = tx.unbounded_send(Msg::Done(Ok(found)));
                        return;
                    }
                    // 3. walking the disk
                    let q = rf_win::search::SearchQuery { root, pattern: query, include_hidden, max_results: 50_000 };
                    let tx2 = tx.clone();
                    let r = rf_win::search::search(&q, &|| cancel.load(Ordering::Relaxed), &mut |hits| {
                        let mut es = Vec::with_capacity(hits.len());
                        let mut ps = Vec::with_capacity(hits.len());
                        for h in hits {
                            let parent: Arc<std::path::Path> = h.path.parent().unwrap_or(&h.path).into();
                            ps.push(parent);
                            es.push(h.entry);
                        }
                        let _ = tx2.unbounded_send(Msg::Hits(es, ps));
                    });
                    let _ = tx.unbounded_send(Msg::Done(r.map(|s| s.hits as usize).map_err(|e| e.to_string())));
                })
                .ok();
        } else {
        let spec = (self.filter.clone(), self.sort, self.group);
        std::thread::Builder::new()
            .name("rf-list".into())
            .spawn(move || {
                let tx2 = tx.clone();
                let mut all: Vec<Entry> = Vec::new();
                let r = match crate::archive_view::split(&path) {
                    // inside an archive: list its entries like a folder
                    // a protected archive asks for its password first (zip lists without one,
                    // but every open/copy would fail later)
                    Some((archive, inner)) => match crate::archive_view::password_gate(&archive) {
                        Some(wrong) => Err(format!("{}{}{}", crate::archive_view::PASSWORD_MARK, wrong as u8, archive.display())),
                        None => crate::archive_view::list_inside(&archive, &inner)
                            .map(|es| {
                                let n = es.len();
                                all.extend(es.iter().cloned());
                                let _ = tx2.unbounded_send(Msg::Batch(es));
                                n
                            })
                            .map_err(|e| match crate::archive_view::password_problem(&e) {
                                Some(wrong) => format!("{}{}{}", crate::archive_view::PASSWORD_MARK, wrong as u8, archive.display()),
                                None => format!("{e:#}"),
                            }),
                    },
                    None => rf_win::list_dir(&path, 512, &|| cancel.load(Ordering::Relaxed), &mut |b| {
                        all.extend(b.iter().cloned());
                        let _ = tx2.unbounded_send(Msg::Batch(b));
                    })
                    .map_err(|e| e.to_string()),
                };
                if r.is_ok() && !cancel.load(Ordering::Relaxed) {
                    let _ = tx.unbounded_send(Msg::View(compute_view(&all, &spec.0, spec.1, spec.2)));
                }
                let _ = tx.unbounded_send(Msg::Done(r.map_err(|e| e.to_string())));
            })
            .ok();
        }

        self._load_task = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut msgs = vec![first];
                while let Ok(m) = rx.try_recv() {
                    msgs.push(m);
                }
                let alive = this.update(cx, |v, cx| v.apply(generation, msgs, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply(&mut self, generation: u64, msgs: Vec<Msg>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        if self.soft_pending.is_some() {
            self.apply_soft(msgs, cx);
            return;
        }
        let mut done = false;
        let mut view = None;
        for m in msgs {
            match m {
                Msg::View(v) => view = Some(v),
                Msg::Batch(b) => {
                    self.entries.extend(b);
                    self.entries_gen += 1;
                }
                Msg::Hits(es, ps) => {
                    self.entries.extend(es);
                    self.entries_gen += 1;
                    self.parents.extend(ps);
                }
                Msg::Shell(es, items) => {
                    self.entries.extend(es);
                    self.entries_gen += 1;
                    self.shell_items.extend(items);
                }
                Msg::ShellName(n) => self.shell_title = Some(n),
                Msg::Done(r) => {
                    done = true;
                    self.loading = false;
                    self.apply_adaptive();
                    self.load_ms = Some(self.load_started.elapsed().as_millis());
                    if trace_live() {
                        log::info!("live: listed {} entries in {} ms", self.entries.len(), self.load_started.elapsed().as_millis());
                    }
                    if let Err(e) = r {
                        if let Some(rest) = e.strip_prefix(crate::archive_view::PASSWORD_MARK) {
                            // "<0|1><archive path>": ask the workspace to prompt for it
                            let wrong = rest.starts_with('1');
                            let archive = PathBuf::from(&rest[1..]);
                            self.error = Some(t!("Bu arşiv parola ile korunuyor").to_string());
                            cx.emit(FolderEvent::NeedPassword(archive, wrong));
                        } else if e != "İptal edildi" {
                            self.error = Some(e);
                        }
                    }
                }
            }
        }
        // Small folders: keep fully sorted while streaming. Huge folders: append now, sort once at the end.
        if done && self.try_install_view(view) {
            // sorted on the listing thread
        } else if done || self.entries.len() <= 20_000 || self.group != GroupBy::None {
            self.rebuild_visible();
        } else {
            let start = self.visible.len() as u32;
            self.visible.extend(start..self.entries.len() as u32);
        }
        if done {
            self.resort_for_sizes(cx);
            self.apply_reselect(cx);
        }
        cx.notify();
    }

    fn apply_reselect(&mut self, cx: &mut Context<Self>) {
        if let Some((names, until)) = self.reselect_many.take() {
            let want: HashSet<&str> = names.iter().map(String::as_str).collect();
            let found: Vec<usize> = self.visible.iter().enumerate().filter(|&(_, &i)| want.contains(&*self.entries[i as usize].name)).map(|(vi, _)| vi).collect();
            if !found.is_empty() {
                self.selected.clear();
                for &vi in &found {
                    self.selected.insert(self.visible[vi]);
                }
                self.cursor = found.first().copied();
                self.anchor = found.first().copied();
                self.scroll.scroll_to_item(self.list_row(found[0]), ScrollStrategy::Center);
                cx.notify();
            }
            // keep waiting for the rest while the watcher catches up
            if found.len() < want.len() && Instant::now() < until {
                self.reselect_many = Some((names, until));
            }
        }
        if let Some(name) = self.reselect.take() {
            if let Some(vi) = self.visible.iter().position(|&i| &*self.entries[i as usize].name == name) {
                self.select_only(vi);
                self.scroll.scroll_to_item(self.list_row(vi), ScrollStrategy::Nearest);
                if std::mem::take(&mut self.rename_after_load) {
                    self.pending_rename = true;
                }
            } else if self.rename_after_load {
                // not there yet (watcher lag) — keep waiting for the next refresh
                self.reselect = Some(name);
            }
        }
        let _ = cx;
    }

    fn apply_soft(&mut self, msgs: Vec<Msg>, cx: &mut Context<Self>) {
        let mut done = false;
        let mut view = None;
        for m in msgs {
            match m {
                Msg::View(v) => view = Some(v),
                Msg::Batch(b) => {
                    if let Some(p) = self.soft_pending.as_mut() {
                        p.extend(b)
                    }
                }
                Msg::Hits(..) | Msg::Shell(..) | Msg::ShellName(_) => {}
                Msg::Done(Err(e)) if e.contains("bulunamadı") => {
                    // The folder we are showing was deleted/renamed away: step up.
                    self.soft_pending = None;
                    if let Some(parent) = self.location().parent() {
                        self.navigate(parent, cx);
                    }
                    return;
                }
                Msg::Done(_) => done = true,
            }
        }
        if !done {
            return;
        }
        let Some(new_entries) = self.soft_pending.take() else { return };
        self.swap_in(new_entries, view, None, cx);
    }

    /// Changed entries read from the disk (the folder watcher): `found` is each changed name
    /// with what is there now (None: gone). The rest of the listing is kept; the sort order is
    /// patched too (no relisting, no sort: a file added to a 100,000-file folder costs a pass).
    fn apply_patch(&mut self, found: Vec<(String, Option<Entry>)>, cx: &mut Context<Self>) {
        fn fold(s: &str) -> std::borrow::Cow<'_, str> {
            if s.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) { s.into() } else { s.to_lowercase().into() }
        }
        if self.git.is_some() {
            self.refresh_git_soon(cx);
        }
        // (Windows names ignore case: a rename that only changes case reports the old spelling)
        let mut gone: HashSet<String> = found.iter().map(|(n, _)| fold(n).into_owned()).collect();
        gone.extend(found.iter().filter_map(|(_, e)| e.as_ref()).map(|e| fold(&e.name).into_owned()));
        let old_n = self.entries.len();
        let mut remap = vec![u32::MAX; old_n];
        let mut fresh = Vec::with_capacity(old_n + found.len());
        for (i, e) in self.entries.iter().enumerate() {
            if gone.contains(fold(&e.name).as_ref()) {
                continue;
            }
            remap[i] = fresh.len() as u32;
            fresh.push(e.clone());
        }
        let kept = fresh.len();
        let mut added = HashSet::new();
        for e in found.into_iter().filter_map(|(_, e)| e) {
            if added.insert(e.name.clone()) {
                fresh.push(e);
            }
        }
        let sorted = match &self.sorted_cache {
            Some((sort, generation, s)) if *sort == self.sort && *generation == self.entries_gen && s.len() == old_n => {
                let mut v: Vec<u32> = s.iter().filter_map(|&i| Some(remap[i as usize]).filter(|&j| j != u32::MAX)).collect();
                for j in kept..fresh.len() {
                    let pos = v.partition_point(|&x| self.sort.compare(&fresh[x as usize], &fresh[j]) == std::cmp::Ordering::Less);
                    v.insert(pos, j as u32);
                }
                Some(v)
            }
            _ => None,
        };
        self.soft_started = Instant::now();
        self.swap_in(fresh, None, sorted, cx);
    }

    /// Puts a new listing of the same folder in place, keeping the selection, the cursor and a
    /// rename by name. `sorted`: the new entries' sort order when already known.
    fn swap_in(&mut self, new_entries: Vec<Entry>, view: Option<View>, sorted: Option<Vec<u32>>, cx: &mut Context<Self>) {
        let (was_selected, applying) = (self.selected.len(), Instant::now());
        let selected_names: HashSet<Arc<str>> =
            self.selected.iter().map(|&i| self.entries[i as usize].name.clone()).collect();
        let cursor_name = self.cursor.and_then(|c| self.visible.get(c)).map(|&i| self.entries[i as usize].name.clone());
        let renaming_name = self.renaming.as_ref().map(|(i, _)| self.entries[*i as usize].name.clone());
        self.entries = new_entries;
        self.entries_gen += 1;
        if let Some(s) = sorted {
            self.sorted_cache = Some((self.sort, self.entries_gen, Arc::new(s)));
        }
        if !self.try_install_view(view) {
            self.rebuild_visible();
        }
        // measured folder sizes go back into the fresh entries (before the cursor is placed)
        self.resort_for_sizes(cx);
        self.selected = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| selected_names.contains(&e.name))
            .map(|(i, _)| i as u32)
            .collect();
        if trace_live() {
            log::info!(
                "live: update of {} entries, relisted in {} ms, applied in {} ms; selected {} of {}, cursor {:?}",
                self.entries.len(),
                self.soft_started.elapsed().as_millis(),
                applying.elapsed().as_millis(),
                self.selected.len(),
                was_selected,
                self.cursor
            );
        }
        // the cursor's file renamed or deleted in the background: keep the place, not lose it
        let old_cursor = self.cursor;
        self.cursor = cursor_name
            .and_then(|n| self.visible.iter().position(|&i| self.entries[i as usize].name == n))
            .or_else(|| old_cursor.filter(|_| !self.visible.is_empty()).map(|c| c.min(self.visible.len() - 1)));
        if let Some(n) = renaming_name {
            match self.entries.iter().position(|e| e.name == n) {
                Some(i) => {
                    if let Some(r) = self.renaming.as_mut() {
                        r.0 = i as u32
                    }
                }
                None => {
                    self.renaming = None;
                    self._rename_sub = None;
                }
            }
        }
        self.apply_reselect(cx);
        cx.notify();
    }

    fn rebuild_visible(&mut self) {
        let view = match &self.sorted_cache {
            // the same entries in the same order: only the filter or grouping changed
            Some((sort, generation, sorted)) if *sort == self.sort && *generation == self.entries_gen && sorted.len() == self.entries.len() => {
                view_from_sorted(&self.entries, sorted.clone(), &self.filter, self.sort, self.group)
            }
            _ => compute_view(&self.entries, &self.filter, self.sort, self.group),
        };
        self.install_view(view);
    }

    /// Uses a view computed elsewhere if it still matches the current entries and settings.
    fn try_install_view(&mut self, view: Option<View>) -> bool {
        match view {
            Some(v) if v.len == self.entries.len() && v.spec == (self.filter.clone(), self.sort, self.group) => {
                self.install_view(v);
                true
            }
            _ => false,
        }
    }

    fn install_view(&mut self, view: View) {
        // (installed only when it matches the current entries: see try_install_view)
        self.sorted_cache = Some((view.spec.1, self.entries_gen, view.sorted));
        self.rows = view.rows;
        self.row_of_vi = view.row_of_vi;
        self.visible = view.visible;
        // Never act on items the user can't see (filtered out / hidden).
        if !self.selected.is_empty() {
            let vis: HashSet<u32> = self.visible.iter().copied().collect();
            self.selected.retain(|e| vis.contains(e));
        }
        if let Some(c) = self.cursor {
            if c >= self.visible.len() {
                self.cursor = None;
            }
        }
    }

    pub fn set_group(&mut self, g: GroupBy, cx: &mut Context<Self>) {
        self.group = g;
        self.rebuild_visible();
        self.remember_layout(cx);
        cx.notify();
    }

    fn layout_now(&self) -> crate::layouts::FolderLayout {
        crate::layouts::FolderLayout {
            view: match self.view_mode {
                ViewMode::Details => 0,
                ViewMode::Grid => match self.grid_style {
                    GridStyle::Tiles => 1,
                    GridStyle::Cards => 3,
                    GridStyle::List => 4,
                },
                ViewMode::Columns => 2,
            },
            sort: self.sort.key,
            desc: self.sort.dir == SortDir::Desc,
            dirs_first: self.sort.dirs_first,
            files_first: self.sort.files_first,
            row_size: self.row_size,
            grid_size: self.grid_size,
            group: match self.group {
                GroupBy::None => 0,
                GroupBy::Name => 1,
                GroupBy::Type => 2,
                GroupBy::Modified => 3,
                GroupBy::Size => 4,
            },
        }
    }

    /// "Make this the default for all folders": the folder's view, sort and grouping become the
    /// Settings defaults (folders with their own remembered layout keep it).
    pub fn make_layout_default(&mut self, cx: &mut Context<Self>) {
        use crate::settings::DefaultView;
        let l = self.layout_now();
        let group = ["none", "name", "type", "modified", "size"][l.group.min(4) as usize].to_string();
        crate::settings::update(cx, move |s| {
            s.default_view = match l.view {
                1 | 3 | 4 => DefaultView::Grid,
                2 => DefaultView::Columns,
                _ => DefaultView::Details,
            };
            s.default_sort = l.sort;
            s.default_sort_desc = l.desc;
            s.folders_first = l.dirs_first;
            s.files_first = l.files_first;
            s.default_group = group;
        });
    }

    /// Per-folder memory: the user changed the view, sort or grouping of this folder.
    fn remember_layout(&mut self, cx: &mut Context<Self>) {
        self.adaptive_pending = false;
        if cx.global::<crate::settings::Settings>().sync_layout {
            return;
        }
        if let Location::Path(p) = self.location().clone() {
            let l = self.layout_now();
            crate::layouts::set(&p, l, cx);
        }
    }

    /// On entering a folder: its remembered layout, else the defaults from Settings (and the
    /// adaptive choice once the listing is in). With "sync layout" on the tab keeps its own.
    fn apply_folder_layout(&mut self, cx: &mut Context<Self>) {
        let s = cx.global::<crate::settings::Settings>().clone();
        if s.sync_layout {
            return;
        }
        let Location::Path(p) = self.location().clone() else { return };
        let (l, adaptive) = match crate::layouts::get(&p, cx) {
            Some(l) => (l, false),
            // Adaptive, like Explorer's folder types: Pictures and Videos as a grid, Downloads
            // newest first in date groups (the rest is decided once the folder is listed)
            None if s.default_view == crate::settings::DefaultView::Adaptive && known_kind(&p).is_some() => {
                let grid = known_kind(&p) != Some(KnownKind::Downloads);
                (
                    crate::layouts::FolderLayout {
                        view: if grid { 1 } else { 0 },
                        sort: if grid { s.default_sort } else { rf_core::SortKey::Modified },
                        desc: if grid { s.default_sort_desc } else { true },
                        dirs_first: s.folders_first,
                        files_first: !s.folders_first && s.files_first,
                        row_size: DEFAULT_ROW_SIZE,
                        grid_size: DEFAULT_GRID_SIZE,
                        group: if grid { crate::settings_view::group_ix(&s.default_group) as u8 } else { 3 },
                    },
                    false,
                )
            }
            None => (
                crate::layouts::FolderLayout {
                    view: match s.default_view {
                        crate::settings::DefaultView::Grid => 1,
                        crate::settings::DefaultView::Columns => 2,
                        _ => 0,
                    },
                    sort: s.default_sort,
                    desc: s.default_sort_desc,
                    dirs_first: s.folders_first,
                    files_first: !s.folders_first && s.files_first,
                    row_size: DEFAULT_ROW_SIZE,
                    grid_size: DEFAULT_GRID_SIZE,
                    group: crate::settings_view::group_ix(&s.default_group) as u8,
                },
                s.default_view == crate::settings::DefaultView::Adaptive,
            ),
        };
        self.view_mode = match l.view {
            1 | 3 | 4 => ViewMode::Grid,
            2 => ViewMode::Columns,
            _ => ViewMode::Details,
        };
        self.grid_style = match l.view {
            3 => GridStyle::Cards,
            4 => GridStyle::List,
            _ => GridStyle::Tiles,
        };
        self.sort = SortSpec { key: l.sort, dir: if l.desc { SortDir::Desc } else { SortDir::Asc }, dirs_first: l.dirs_first, files_first: l.files_first };
        self.row_size = l.row_size.min(ROW_SIZES.len() as u8 - 1);
        self.grid_size = l.grid_size.min(GRID_SIZES.len() as u8 - 1);
        self.group = match l.group {
            1 => GroupBy::Name,
            2 => GroupBy::Type,
            3 => GroupBy::Modified,
            4 => GroupBy::Size,
            _ => GroupBy::None,
        };
        self.adaptive_pending = adaptive;
    }

}

/// Known folders with a layout of their own in adaptive mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownKind {
    Pictures,
    Downloads,
}

/// Pictures and Videos (and folders inside them), or Downloads itself.
fn known_kind(p: &std::path::Path) -> Option<KnownKind> {
    use rf_win::known::KnownFolder;
    static PATHS: std::sync::OnceLock<Vec<(std::path::PathBuf, KnownKind)>> = std::sync::OnceLock::new();
    let paths = PATHS.get_or_init(|| {
        [(KnownFolder::Pictures, KnownKind::Pictures), (KnownFolder::Videos, KnownKind::Pictures), (KnownFolder::Downloads, KnownKind::Downloads)]
            .into_iter()
            .filter_map(|(k, kind)| k.path().map(|p| (p, kind)))
            .collect()
    });
    let lower = |q: &std::path::Path| q.to_string_lossy().to_lowercase();
    let here = lower(p);
    paths.iter().find_map(|(root, kind)| {
        let r = lower(root);
        match kind {
            KnownKind::Downloads => (here == r).then_some(*kind),
            KnownKind::Pictures => (here == r || here.starts_with(&format!("{r}\\"))).then_some(*kind),
        }
    })
}

impl FolderView {
    /// Adaptive layout: a folder of mostly pictures and videos opens as a grid.
    fn apply_adaptive(&mut self) {
        if !self.adaptive_pending {
            return;
        }
        self.adaptive_pending = false;
        let files: Vec<&Entry> = self.entries.iter().filter(|e| !e.is_dir()).collect();
        let media = files
            .iter()
            .filter(|e| matches!(icons::category(e), icons::Category::Image | icons::Category::Video))
            .count();
        if files.len() >= 4 && media * 2 >= files.len() {
            self.view_mode = ViewMode::Grid;
            self.grid_style = GridStyle::Tiles;
        }
    }

    fn grouped(&self) -> bool {
        self.view_mode == ViewMode::Details && !self.rows.is_empty()
    }

    /// Details-list row for a visible index (accounts for group headers).
    fn list_row(&self, vi: usize) -> usize {
        if self.grouped() { self.row_of_vi.get(vi).copied().unwrap_or(vi) } else { vi }
    }

    pub fn set_query(&mut self, q: String, cx: &mut Context<Self>) {
        if self.filter.query != q {
            self.filter.query = q;
            self.rebuild_visible();
            cx.notify();
        }
    }

    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        let v = !self.filter.show_hidden;
        crate::settings::update(cx, |s| s.show_hidden = v);
    }

    pub fn set_sort_spec(&mut self, spec: SortSpec, cx: &mut Context<Self>) {
        self.sort = spec;
        self.rebuild_visible();
        self.remember_layout(cx);
        cx.notify();
    }

    pub fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled(key);
        self.rebuild_visible();
        self.remember_layout(cx);
        cx.notify();
    }

    // ---------- selection ----------

    fn select_only(&mut self, vi: usize) {
        self.selected.clear();
        if let Some(&ei) = self.visible.get(vi) {
            self.selected.insert(ei);
            self.cursor = Some(vi);
            self.anchor = Some(vi);
        }
    }

    fn select_range(&mut self, from: usize, to: usize) {
        self.selected.clear();
        let (a, b) = if from <= to { (from, to) } else { (to, from) };
        for vi in a..=b.min(self.visible.len().saturating_sub(1)) {
            self.selected.insert(self.visible[vi]);
        }
        self.cursor = Some(to);
    }

    fn move_cursor(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() as isize - 1;
        let next = match self.cursor {
            None => {
                if delta >= 0 {
                    0
                } else {
                    last
                }
            }
            Some(c) => (c as isize + delta).clamp(0, last),
        } as usize;
        if extend {
            let anchor = self.anchor.unwrap_or(next);
            self.anchor = Some(anchor);
            self.select_range(anchor, next);
        } else {
            self.select_only(next);
        }
        let row = if self.view_mode == ViewMode::Grid { next / self.grid_cols.max(1) } else { self.list_row(next) };
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn grid_tile_w(&self) -> Pixels {
        match self.grid_style {
            GridStyle::Tiles => px(GRID_SIZES[self.grid_size as usize].0),
            GridStyle::Cards => px(300.),
            GridStyle::List => px(240.),
        }
    }
    fn grid_row_h(&self) -> Pixels {
        match self.grid_style {
            GridStyle::Tiles => px(GRID_SIZES[self.grid_size as usize].1),
            GridStyle::Cards => px(84.),
            GridStyle::List => px(32.),
        }
    }
    fn grid_thumb_px(&self) -> u32 {
        match self.grid_style {
            GridStyle::Tiles => GRID_SIZES[self.grid_size as usize].2,
            GridStyle::Cards => 96,
            GridStyle::List => 32,
        }
    }

    pub fn row_h(&self) -> Pixels {
        px(ROW_SIZES[(self.row_size as usize).min(ROW_SIZES.len() - 1)])
    }

    /// The "Size" slider's position for the current layout (`None`: it has no sizes).
    pub fn layout_size(&self) -> Option<u8> {
        match (self.view_mode, self.grid_style) {
            (ViewMode::Details, _) => Some(self.row_size),
            (ViewMode::Grid, GridStyle::Tiles) => Some(self.grid_size),
            _ => None,
        }
    }

    pub fn set_layout_size(&mut self, n: u8, cx: &mut Context<Self>) {
        match (self.view_mode, self.grid_style) {
            (ViewMode::Details, _) => self.row_size = n.min(ROW_SIZES.len() as u8 - 1),
            (ViewMode::Grid, GridStyle::Tiles) => self.grid_size = n.min(GRID_SIZES.len() as u8 - 1),
            _ => return,
        }
        self.remember_layout(cx);
        cx.notify();
    }

    pub fn zoom(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.view_mode != ViewMode::Grid || self.grid_style != GridStyle::Tiles {
            self.set_view_mode(ViewMode::Grid, cx);
            return;
        }
        let n = (self.grid_size as i32 + delta).clamp(0, GRID_SIZES.len() as i32 - 1) as u8;
        if n != self.grid_size {
            self.grid_size = n;
            self.remember_layout(cx);
            cx.notify();
        }
    }

    fn row_step(&self) -> isize {
        if self.view_mode == ViewMode::Grid { self.grid_cols.max(1) as isize } else { 1 }
    }

    pub fn set_view_mode(&mut self, m: ViewMode, cx: &mut Context<Self>) {
        self.set_layout(m, GridStyle::Tiles, cx)
    }

    /// View + grid look (Cards and List are grid styles).
    pub fn set_layout(&mut self, m: ViewMode, style: GridStyle, cx: &mut Context<Self>) {
        let style = if m == ViewMode::Grid { style } else { self.grid_style };
        if self.view_mode != m || self.grid_style != style {
            self.view_mode = m;
            self.grid_style = style;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.remember_layout(cx);
            cx.notify();
        }
    }

    fn render_grid_rows(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = Palette::for_window(window);
        let cols = self.grid_cols.max(1);
        let (tile_w, row_h, thumb_px) = (self.grid_tile_w(), self.grid_row_h(), self.grid_thumb_px());
        let style = self.grid_style;
        let visual_px = match style {
            GridStyle::Tiles => px(thumb_px as f32 * 0.75),
            GridStyle::Cards => px(60.),
            GridStyle::List => px(20.),
        };
        let show_ext = cx.global::<crate::settings::Settings>().show_extensions;
        let drop_bg = p.selection;
        let drop_border = p.accent;
        let mut out = Vec::with_capacity(range.len());
        for row in range {
            let mut r = h_flex().w_full().h(self.grid_row_h()).gap(GRID_GAP).items_start();
            for c in 0..cols {
                let vi = row * cols + c;
                let Some(&ei) = self.visible.get(vi) else { break };
                let e = &self.entries[ei as usize];
                let selected = self.selected.contains(&ei);
                let cat = icons::category(e);
                let dim = e.is_hidden() || e.is_cloud_placeholder();
                // Settings → "Show thumbnails" off: the file type's icon instead of a preview
                let thumbs_on = cx.global::<crate::settings::Settings>().show_thumbnails;
                let thumb = self.parent_of(ei).map(|d| d.to_path_buf()).and_then(|d| {
                    self.images.update(cx, |c, cx| if thumbs_on { c.thumbnail(&d, e, thumb_px, cx) } else { c.icon(&d, e, cx) })
                });
                let is_thumb = icons::has_thumbnail(e);
                let visual: AnyElement = match thumb {
                    Some(im) => img(im)
                        .size(visual_px)
                        .object_fit(ObjectFit::Contain)
                        .when(is_thumb, |i| i.rounded(px(4.)))
                        .into_any_element(),
                    None => Icon::new(icons::icon_for(cat))
                        .size(visual_px * 0.6)
                        .text_color(theme::kind_color(cat, p.dark))
                        .into_any_element(),
                };
                let rename_input = self.renaming.as_ref().filter(|(i, _)| *i == ei).map(|(_, inp)| inp.clone());
                let label: AnyElement = match rename_input {
                    // the text stays where the label was: centered, the label's size
                    Some(inp) => div()
                        .w_full()
                        .h(px(24.))
                        // the label's line is shorter than the box: center the box on it
                        .mt(px(-4.))
                        .rounded(px(4.))
                        .bg(p.card_bg)
                        .border_1()
                        .border_color(p.accent)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(gpui_kit::component::input::Input::new(&inp).appearance(false).small().px_1().text_xs().text_center())
                        .into_any_element(),
                    None => div()
                        .w_full()
                        .text_xs()
                        .text_center()
                        .line_clamp(2)
                        .when(dim, |d| d.opacity(0.6))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.name_hit = true))
                        .child(SharedString::from(shown_name(e, show_ext)))
                        .into_any_element(),
                };
                let facts = (style == GridStyle::Cards).then(|| {
                    let mut f = vec![icons::type_label(e).to_string()];
                    if !e.is_dir() {
                        f.push(format::size(e.size));
                    }
                    f.push(format::datetime(e.modified_ms));
                    f
                });
                let name_text = SharedString::from(shown_name(e, show_ext));
                let base = match style {
                    GridStyle::Tiles => v_flex().p_1().gap_1().items_center().child(div().size(visual_px + px(4.)).flex().items_center().justify_center().child(visual)).child(label),
                    GridStyle::Cards => h_flex()
                        .px_2()
                        .gap_3()
                        .items_center()
                        .child(div().size(visual_px + px(4.)).flex_shrink_0().flex().items_center().justify_center().child(visual))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).whitespace_nowrap().overflow_hidden().text_ellipsis().when(dim, |d| d.opacity(0.6)).on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.name_hit = true)).child(name_text.clone()))
                                .children(facts.unwrap_or_default().into_iter().map(|t| div().text_xs().text_color(p.text_secondary).whitespace_nowrap().overflow_hidden().text_ellipsis().child(t))),
                        ),
                    GridStyle::List => h_flex()
                        .px_2()
                        .gap_2()
                        .items_center()
                        .child(div().size(visual_px).flex_shrink_0().flex().items_center().justify_center().child(visual))
                        .child(div().flex_1().min_w_0().text_sm().whitespace_nowrap().overflow_hidden().text_ellipsis().when(dim, |d| d.opacity(0.6)).on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.name_hit = true)).child(name_text.clone())),
                };
                let renaming_here = self.renaming.as_ref().is_some_and(|(i, _)| *i == ei);
                let base = if style != GridStyle::Tiles && renaming_here {
                    // the rename box replaces the name line
                    let inp = self.renaming.as_ref().map(|(_, i)| i.clone()).unwrap();
                    h_flex().px_2().gap_2().items_center().child(div().size(visual_px).flex_shrink_0()).child(
                        // the box grows to the left (border + padding) so the text keeps its place
                        div()
                            .flex_1()
                            .h(px(24.))
                            .ml(px(-RENAME_INSET))
                            .px(px(RENAME_INSET - 1.))
                            .rounded(px(4.))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(p.accent)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(gpui_kit::component::input::Input::new(&inp).appearance(false).small().px_0()),
                    )
                } else {
                    base
                };
                let cell = base
                    .id(("cell", vi))
                    .w(tile_w)
                    .h(row_h - px(4.))
                    .rounded(theme::RADIUS)
                    // tiles: quiet fill + a thin accent outline (a pill would read as a crop)
                    .when(selected, |d| d.bg(p.selection).border_1().border_color(p.selection_border))
                    .when(!selected, |d| d.border_1().border_color(transparent_black()))
                    .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| this.on_row_mouse_down(vi, ev, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| this.on_row_right_down(vi, window, cx)),
                    )
                    .when(e.is_dir(), |d| {
                        d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg).border_1().border_color(drop_border))
                            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                                if let Some(dest) = this.drop_target_row(ei) {
                                    this.drop_into(dest, paths, cx);
                                }
                            }))
                    });
                r = r.child(cell);
            }
            out.push(r.into_any_element());
        }
        out
    }

    fn on_row_mouse_down(&mut self, vi: usize, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // before anything changes: was this the only selected item, in a focused, active list?
        let on_name = std::mem::take(&mut self.name_hit);
        let was_renaming = self.renaming.is_some();
        let had_focus = window.is_window_active() && self.focus.is_focused(window);
        let was_only = self.visible.get(vi).is_some_and(|ei| self.selected.len() == 1 && self.selected.contains(ei));
        self.slow_click = None;
        if self.renaming.is_some() {
            self.cancel_rename(window, cx);
        }
        window.focus(&self.focus, cx);
        let Some(&ei) = self.visible.get(vi) else { return };
        let plain = !ev.modifiers.control && !ev.modifiers.shift;
        // Settings → "Open items with a single click"
        let single = match cx.global::<crate::settings::Settings>().single_click {
            crate::settings::ClickPref::Never => false,
            crate::settings::ClickPref::Folders => self.entries[ei as usize].is_dir(),
            crate::settings::ClickPref::Always => true,
        };
        if plain && (ev.click_count >= 2 || (single && ev.click_count == 1)) {
            self.select_only(vi);
            self.open_entry(ei, cx);
            return;
        }
        if ev.modifiers.shift {
            let anchor = self.anchor.unwrap_or(vi);
            self.select_range(anchor, vi);
            self.anchor = Some(anchor);
        } else if ev.modifiers.control {
            if !self.selected.remove(&ei) {
                self.selected.insert(ei);
            }
            self.cursor = Some(vi);
            self.anchor = Some(vi);
        } else if !self.selected.contains(&ei) {
            self.select_only(vi);
        } else {
            // Clicking inside an existing multi-selection keeps it (so it can be dragged).
            self.cursor = Some(vi);
        }
        self.drag_origin = Some(ev.position);
        // Explorer: a click on the name of the item that was already the only one selected
        // renames it, once the double-click time passes without a second click or a drag
        if plain && ev.click_count == 1 && on_name && was_only && had_focus && !was_renaming {
            self.arm_slow_click(ei, cx);
        }
        cx.notify();
    }

    fn arm_slow_click(&mut self, ei: u32, cx: &mut Context<Self>) {
        self.slow_click_serial += 1;
        let serial = self.slow_click_serial;
        self.slow_click = Some((ei, serial));
        let wait = std::time::Duration::from_millis(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() } as u64);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |v, cx| {
                if v.slow_click != Some((ei, serial)) {
                    return;
                }
                v.slow_click = None;
                // the selection must still be just this item, under the cursor
                let still = v.selected.len() == 1 && v.selected.contains(&ei) && v.cursor_entry() == Some(ei);
                if still && v.renaming.is_none() {
                    v.request_rename();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_drag_motion(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(start) = self.drag_origin else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag_origin = None;
            return;
        }
        let d = ev.position - start;
        if d.x.abs() + d.y.abs() > px(8.) {
            self.drag_origin = None;
            self.slow_click = None;
            self.start_drag_out(window, cx);
        }
    }

    /// Window position → coordinates inside the scrolled list content.
    fn content_point(&self, pos: Point<Pixels>) -> Point<Pixels> {
        let st = self.scroll.0.borrow();
        let b = st.base_handle.bounds();
        let off = st.base_handle.offset();
        point(pos.x - b.origin.x - off.x, pos.y - b.origin.y - off.y)
    }

    fn begin_marquee(&mut self, ev: &MouseDownEvent) {
        let p = self.content_point(ev.position);
        let base = if ev.modifiers.control || ev.modifiers.shift { self.selected.clone() } else { HashSet::new() };
        self.marquee = Some(Marquee { start: p, cur: p, base });
    }

    fn update_marquee(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let cur = self.content_point(pos);
        let Some(m) = self.marquee.as_mut() else { return };
        m.cur = cur;
        let (x0, x1) = (f32::from(m.start.x.min(cur.x)), f32::from(m.start.x.max(cur.x)));
        let (y0, y1) = (f32::from(m.start.y.min(cur.y)), f32::from(m.start.y.max(cur.y)));
        let mut sel = m.base.clone();
        let n = self.visible.len();
        if n > 0 && (y1 - y0 > 2.0 || x1 - x0 > 2.0) {
            match self.view_mode {
                ViewMode::Details => {
                    let pad = 4.0;
                    let h = f32::from(self.row_h());
                    if y1 >= pad {
                        let rows = if self.grouped() { self.rows.len() } else { n };
                        let first = ((y0 - pad).max(0.0) / h) as usize;
                        let last = (((y1 - pad) / h) as usize).min(rows - 1);
                        for r in first..=last.max(first).min(rows - 1) {
                            let vi = if self.grouped() {
                                match self.rows[r] {
                                    Row::Item(vi) => vi,
                                    Row::Header(..) => continue,
                                }
                            } else {
                                r
                            };
                            if let Some(&ei) = self.visible.get(vi) {
                                sel.insert(ei);
                            }
                        }
                    }
                }
                ViewMode::Columns => {}
                ViewMode::Grid => {
                    let (pt, pl) = (8.0, 6.0);
                    let cw = f32::from(self.grid_tile_w() + GRID_GAP);
                    let rh = f32::from(self.grid_row_h());
                    let cols = self.grid_cols.max(1);
                    if y1 >= pt && x1 >= pl {
                        let r0 = ((y0 - pt).max(0.0) / rh) as usize;
                        let r1 = ((y1 - pt) / rh) as usize;
                        let c0 = ((x0 - pl).max(0.0) / cw) as usize;
                        let c1 = (((x1 - pl) / cw) as usize).min(cols - 1);
                        for r in r0..=r1 {
                            for c in c0..=c1 {
                                let vi = r * cols + c;
                                if vi < n {
                                    sel.insert(self.visible[vi]);
                                }
                            }
                        }
                    }
                }
            }
        }
        self.selected = sel;
        cx.notify();
    }

    fn render_marquee(&self, p: &Palette) -> Option<AnyElement> {
        let m = self.marquee.as_ref()?;
        let off = self.scroll.0.borrow().base_handle.offset();
        let x0 = m.start.x.min(m.cur.x) + off.x;
        let y0 = m.start.y.min(m.cur.y) + off.y;
        let w = (m.start.x - m.cur.x).abs();
        let h = (m.start.y - m.cur.y).abs();
        if w < px(3.) && h < px(3.) {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(x0)
                .top(y0)
                .w(w)
                .h(h)
                .bg(p.selection)
                .border_1()
                .border_color(p.selection_border)
                .into_any_element(),
        )
    }

    /// Space held to peek (QuickLook's "hold to preview"): released after a long press, the
    /// preview it opened closes. A short press keeps toggling as before.
    fn on_key_up(&mut self, ev: &KeyUpEvent, cx: &mut Context<Self>) {
        if ev.keystroke.key != "space" {
            return;
        }
        let Some(pressed) = self.space_hold.take() else { return };
        if pressed.elapsed() >= std::time::Duration::from_millis(400)
            && QUICK_LOOK_OPEN.load(Ordering::Relaxed)
            && crate::look::config::LookSettings::load().look_hold_to_close
        {
            cx.emit(FolderEvent::QuickLook);
        }
    }

    /// Explorer-style type-ahead: typing selects the next item whose name starts with the typed text.
    fn on_typeahead(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        // Ctrl+Alt together is AltGr on Windows (Turkish Q: AltGr+ı = i, AltGr+Q = @): that is text
        let altgr = m.control && m.alt;
        if ((m.control || m.alt) && !altgr) || m.platform || self.renaming.is_some() {
            return;
        }
        let now = Instant::now();
        // Space: Quick Look — unless a type-ahead name is being typed ("my file")
        if ev.keystroke.key == "space" && !m.shift {
            let typing = !self.typeahead.0.is_empty() && now.duration_since(self.typeahead.1) <= std::time::Duration::from_millis(900);
            if !typing {
                // holding Space repeats the key: one toggle per press
                if ev.is_held {
                    cx.stop_propagation();
                    return;
                }
                if self.cursor_entry().is_some() || QUICK_LOOK_OPEN.load(Ordering::Relaxed) {
                    self.space_hold = (!QUICK_LOOK_OPEN.load(Ordering::Relaxed)).then_some(now);
                    cx.emit(FolderEvent::QuickLook);
                    cx.stop_propagation();
                }
                return;
            }
        }
        let Some(ch) = ev.keystroke.key_char.as_ref().filter(|c| !c.chars().any(|c| c.is_control())) else { return };
        if ch.is_empty() || self.visible.is_empty() {
            return;
        }
        let fresh = now.duration_since(self.typeahead.1) > std::time::Duration::from_millis(900);
        if fresh {
            self.typeahead.0.clear();
        }
        self.typeahead.0.push_str(&rf_core::entry::fold_case(ch));
        self.typeahead.1 = now;
        let needle = self.typeahead.0.clone();
        // Repeating one letter cycles through matches; otherwise search from the cursor.
        let single_repeat = needle.chars().count() > 1 && needle.chars().all(|c| Some(c) == needle.chars().next());
        let needle = if single_repeat { needle.chars().next().unwrap().to_string() } else { needle };
        let n = self.visible.len();
        let start = match self.cursor {
            Some(c) if fresh || single_repeat => c + 1,
            Some(c) => c,
            None => 0,
        };
        for k in 0..n {
            let vi = (start + k) % n;
            let name = rf_core::entry::fold_case(self.entries[self.visible[vi] as usize].shown());
            if name.starts_with(&needle) {
                self.select_only(vi);
                let row = if self.view_mode == ViewMode::Grid { vi / self.grid_cols.max(1) } else { self.list_row(vi) };
                self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
    }

    fn drop_target_row(&self, ei: u32) -> Option<PathBuf> {
        // `ei` was captured at render time; the listing may have changed since.
        let e = self.entries.get(ei as usize)?;
        if e.is_dir() { self.path_of(ei) } else { None }
    }

    pub fn selection_paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<u32> = self.selected.iter().copied().collect();
        v.sort_unstable();
        v.into_iter().filter_map(|i| self.path_of(i)).collect()
    }

    /// The selection, each with whether it is a folder (no disk access).
    pub fn selection_with_kind(&self) -> Vec<(PathBuf, bool)> {
        let mut v: Vec<u32> = self.selected.iter().copied().collect();
        v.sort_unstable();
        v.into_iter().filter_map(|i| self.path_of(i).map(|p| (p, self.entries[i as usize].is_dir()))).collect()
    }

    pub fn item_count(&self) -> usize {
        self.visible.len()
    }

    pub fn selection_summary(&self) -> (usize, u64, bool) {
        let mut size = 0;
        let mut has_dir = false;
        for &i in &self.selected {
            let e = &self.entries[i as usize];
            size += e.size;
            has_dir |= e.is_dir();
        }
        (self.selected.len(), size, has_dir)
    }

    /// Folders whose size was measured carry it in their entry, so sorting by Size puts them
    /// among the files by their real size. Returns whether anything changed.
    fn apply_dir_sizes(&mut self, on_disk: bool) -> bool {
        if self.folder_sizes.is_empty() {
            return false;
        }
        let mut changed = false;
        for i in 0..self.entries.len() {
            if !self.entries[i].is_dir() {
                continue;
            }
            let Some(p) = self.path_of(i as u32) else { continue };
            if let Some(DirSize::Done(s)) = self.folder_sizes.get(&p) {
                let b = DirSize::bytes(*s, on_disk);
                if self.entries[i].size != b {
                    self.entries[i].size = b;
                    self.entries_gen += 1;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Re-sorts when measured folder sizes changed what a Size sort shows.
    fn resort_for_sizes(&mut self, cx: &App) {
        let on_disk = cx.global::<crate::settings::Settings>().folder_size_on_disk;
        if self.apply_dir_sizes(on_disk) && self.sort.key == SortKey::Size {
            self.rebuild_visible();
        }
    }

    /// Narrow panes (dual pane, small windows) drop columns: Type first, then Date.
    /// Size of folder `p` as far as known; asks for it when it was never requested. Requests of
    /// one frame start together as one batch (all folders walk in parallel, each shows up as soon
    /// as it is done; running totals in between).
    fn folder_size(&mut self, p: PathBuf, cx: &mut Context<Self>) -> DirSize {
        if let Some(v) = self.folder_sizes.get(&p) {
            return *v;
        }
        self.folder_sizes.insert(p.clone(), DirSize::Partial(0));
        if self.size_queue.is_empty() {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, |v, cx| v.start_size_batch(cx));
            });
        }
        self.size_queue.push(p);
        DirSize::Partial(0)
    }

    fn start_size_batch(&mut self, cx: &mut Context<Self>) {
        let dirs = std::mem::take(&mut self.size_queue);
        if dirs.is_empty() {
            return;
        }
        self.size_batches.push((rf_win::foldersize::SizeBatch::start(dirs.clone()), dirs));
        if self.size_polling {
            return;
        }
        self.size_polling = true;
        cx.spawn(async move |this, cx| {
            // quick first look (small folders and cache hits are done within milliseconds),
            // then running totals ten times a second
            let mut wait = std::time::Duration::from_millis(15);
            loop {
                cx.background_executor().timer(wait).await;
                wait = std::time::Duration::from_millis(100);
                let more = this
                    .update(cx, |v, cx| {
                        let (mut changed, mut finished) = (false, false);
                        for (b, dirs) in &v.size_batches {
                            for (i, d) in dirs.iter().enumerate() {
                                let now = DirSize::from(b.state(i));
                                if v.folder_sizes.get(d) != Some(&now) {
                                    finished |= matches!(now, DirSize::Done(_));
                                    v.folder_sizes.insert(d.clone(), now);
                                    changed = true;
                                }
                            }
                        }
                        v.size_batches.retain(|(b, _)| !b.finished());
                        if changed {
                            // running totals only redraw; a finished size goes into the entries
                            // (a pass over every folder of the listing), not ten times a second
                            if finished {
                                v.resort_for_sizes(cx);
                            }
                            cx.notify();
                        }
                        let more = !v.size_batches.is_empty();
                        v.size_polling = more;
                        more
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    fn type_col_w(&self, cx: &App) -> Pixels {
        if self.is_search() {
            // location shares the free width with the name instead of squeezing it
            let free = f32::from(self.content_width) - 150. - 100. - 40.;
            px((free * 0.45).clamp(160., 460.))
        } else {
            px(self.col_widths(cx)[2])
        }
    }

    /// Results from many folders (search, tag): entries carry their own parent folder.
    pub fn is_search(&self) -> bool {
        matches!(self.location(), Location::Search { .. } | Location::Tag(_) | Location::Library { .. })
    }

    /// Folder containing entry `ei` (differs per entry in search results).
    pub(crate) fn parent_of(&self, ei: u32) -> Option<&std::path::Path> {
        if matches!(self.location(), Location::Shell { .. }) {
            return self.shell_items.get(ei as usize)?.fs_path.as_deref()?.parent();
        }
        match self.parents.get(ei as usize) {
            Some(p) => Some(p),
            None => self.location().path(),
        }
    }

    pub(crate) fn path_of(&self, ei: u32) -> Option<PathBuf> {
        if matches!(self.location(), Location::Shell { .. }) {
            // only items that are real files have a path; a phone's items have none
            return self.shell_items.get(ei as usize)?.fs_path.clone();
        }
        Some(self.parent_of(ei)?.join(&*self.entries.get(ei as usize)?.name))
    }

    pub(crate) fn places_pinned_paths(&self, cx: &App) -> Vec<PathBuf> {
        self.places.read(cx).pinned.iter().map(|p| p.path.clone()).collect()
    }

    pub(crate) fn refresh_places(&self, cx: &mut Context<Self>) {
        self.places.update(cx, |p, cx| p.refresh(cx));
    }

    /// Item shown in the preview pane.
    pub fn preview_target(&self) -> Option<(PathBuf, Entry)> {
        let ei = self.cursor_entry()?;
        let e = self.entries.get(ei as usize)?;
        Some((self.path_of(ei)?, e.clone()))
    }

    pub(crate) fn entry(&self, ei: u32) -> &Entry {
        &self.entries[ei as usize]
    }

    /// Entry under the keyboard cursor, or the single selected entry.
    pub(crate) fn cursor_entry(&self) -> Option<u32> {
        self.cursor
            .and_then(|c| self.visible.get(c).copied())
            .filter(|ei| self.selected.contains(ei))
            .or_else(|| (self.selected.len() == 1).then(|| *self.selected.iter().next().unwrap()))
    }

    pub(crate) fn set_renaming(
        &mut self,
        r: Option<(u32, Entity<gpui_kit::component::input::InputState>)>,
        sub: Option<Subscription>,
    ) {
        self.renaming = r;
        self._rename_sub = sub;
    }

    /// Select these names (in the current folder) as soon as they're listed: a paste or a copy
    /// into this folder shows what arrived, like Explorer does.
    pub(crate) fn select_when_listed(&mut self, names: Vec<String>, cx: &mut Context<Self>) {
        if names.is_empty() {
            return;
        }
        self.reselect_many = Some((names, Instant::now() + std::time::Duration::from_secs(6)));
        self.soft_reload(cx);
    }

    pub(crate) fn set_reselect(&mut self, name: String) {
        self.reselect = Some(name);
    }

    /// Selects `name` and scrolls to it: now when the folder is listed, or once it is.
    pub(crate) fn select_now(&mut self, name: String, cx: &mut Context<Self>) {
        self.reselect = Some(name);
        if !self.loading {
            self.apply_reselect(cx);
        }
        cx.notify();
    }

    pub(crate) fn clear_reselect(&mut self) {
        self.reselect = None;
        self.rename_after_load = false;
    }

    /// Open/Save dialog mode: which files show (patterns like "*.txt;*.md", empty = all; none
    /// when picking a folder), and opening a file picks it.
    pub(crate) fn set_picking(&mut self, patterns: String, folders_only: bool, cx: &mut Context<Self>) {
        self.picking = true;
        if self.filter.patterns != patterns || self.filter.folders_only != folders_only {
            self.filter.patterns = patterns;
            self.filter.folders_only = folders_only;
            self.rebuild_visible();
            cx.notify();
        }
    }

    pub(crate) fn request_rename(&mut self) {
        self.pending_rename = true;
    }

    /// A new item was created; select it and start rename as soon as the watcher lists it.
    pub(crate) fn expect_new_item(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        if let Some(n) = name {
            self.reselect = Some(n);
            self.rename_after_load = true;
            self.soft_reload(cx);
        }
    }

    fn on_row_right_down(&mut self, vi: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        if let Some(&ei) = self.visible.get(vi) {
            if !self.selected.contains(&ei) {
                self.select_only(vi);
            }
        }
        cx.notify();
        self.show_context_menu(false, window, cx);
    }

    // ---------- opening ----------

    /// Shell folders (phones…): opens what only the shell can open. Returns false for items
    /// that are real files or folders (they go the ordinary way through `path_of`).
    fn open_shell_entry(&mut self, ei: u32, cx: &mut Context<Self>) -> bool {
        let Some(child) = self.shell_items.get(ei as usize).cloned() else { return true };
        let loc = if child.is_folder {
            match &child.fs_path {
                Some(p) if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("library-ms")) => {
                    Location::Library { file: p.clone(), name: child.name.clone() }
                }
                Some(_) => return false,
                // a network computer (`\nas`) is a path to us, the shell lists its shares
                None if child.parsing.starts_with(r"\\") => Location::Path(PathBuf::from(&child.parsing)),
                None => Location::Shell { parsing: child.parsing.clone(), name: child.name.clone() },
            }
        } else {
            if child.fs_path.is_some() {
                return false;
            }
            // an item on a phone: the shell fetches it and opens it with its default app
            // (a dialog asking for a file cannot be answered with one)
            if !self.picking {
                let parsing = child.parsing;
                std::thread::spawn(move || {
                    if let Err(e) = rf_win::shellfolder::open(&parsing) {
                        log::warn!("open {parsing}: {e:#}");
                    }
                });
            }
            return true;
        };
        if cx.global::<crate::settings::Settings>().open_folders_in_new_tab {
            cx.emit(FolderEvent::OpenInNewTab(loc));
        } else {
            self.navigate(loc, cx);
        }
        true
    }

    /// The selected items of a shell folder, in list order.
    pub(crate) fn selection_shell(&self) -> Vec<rf_win::shellfolder::ShellChild> {
        let mut v: Vec<u32> = self.selected.iter().copied().collect();
        v.sort_unstable();
        v.into_iter().filter_map(|i| self.shell_items.get(i as usize).cloned()).collect()
    }

    fn open_entry(&mut self, ei: u32, cx: &mut Context<Self>) {
        if matches!(self.location(), Location::Shell { .. }) && self.open_shell_entry(ei, cx) {
            return;
        }
        let Some(path) = self.path_of(ei) else { return };
        let e = &self.entries[ei as usize];
        let browse = !e.is_dir()
            && cx.global::<crate::settings::Settings>().browse_archives
            && crate::archive_view::is_browsable(&path);
        if e.is_dir() || browse {
            if cx.global::<crate::settings::Settings>().open_folders_in_new_tab {
                cx.emit(FolderEvent::OpenInNewTab(Location::Path(path)));
            } else {
                self.navigate(Location::Path(path), cx);
            }
        } else if self.picking {
            cx.emit(FolderEvent::Picked(vec![path]));
        } else {
            open_in_background(path, cx);
        }
    }

    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        let sel: Vec<u32> = match self.cursor.and_then(|c| self.visible.get(c)).copied() {
            Some(c) if self.selected.len() <= 1 => vec![c],
            _ => self.selected.iter().copied().collect(),
        };
        if sel.len() == 1 {
            self.open_entry(sel[0], cx);
        } else if self.picking {
            let files: Vec<PathBuf> = sel.iter().filter(|&&i| !self.entries[i as usize].is_dir()).filter_map(|&i| self.path_of(i)).collect();
            if !files.is_empty() {
                cx.emit(FolderEvent::Picked(files));
            }
        } else {
            let mut in_archive = Vec::new();
            for i in sel {
                if !self.entries[i as usize].is_dir() {
                    if let Some(p) = self.path_of(i) {
                        if !p.exists() && crate::archive_view::split(&p).is_some() {
                            // extracted together, one Status Center entry
                            in_archive.push(p);
                        } else {
                            open_in_background(p, cx);
                        }
                    }
                }
            }
            crate::archive_view::open_items(cx, in_archive);
        }
    }

    // ---------- rendering ----------

    /// Column widths (modified, created, type, size, tags, git, sync): the live drag, else Settings.
    /// …then the commit columns (message, author, date, hash) at 7..11.
    fn col_widths(&self, cx: &App) -> [f32; 11] {
        self.col_live.unwrap_or_else(|| {
            let c = &cx.global::<crate::settings::Settings>().columns;
            let (a, b, g) = (c.widths, c.extra_widths, c.git_widths);
            [a[0], a[1], a[2], a[3], b[0], b[1], b[2], g[0], g[1], g[2], g[3]].map(f32::from)
        })
    }

    /// Which Details columns show: the ones turned on that make sense here (git inside a
    /// repository, sync with cloud files, location in search), then as many as fit the pane
    /// in priority order — the name column keeps at least 180 px.
    fn visible_cols(&self, cx: &App) -> Cols {
        let c = &cx.global::<crate::settings::Settings>().columns;
        const PINNED: u32 = 0x80000;
        let cloud = c.sync && self.location().path().is_some() && {
            match self.cloud_cache.get() {
                Some((g, any)) if g == self.entries_gen => any,
                _ => {
                    let any = self.entries.iter().any(|e| e.is_cloud_placeholder() || e.attrs & PINNED != 0);
                    self.cloud_cache.set(Some((self.entries_gen, any)));
                    any
                }
            }
        };
        let w = self.col_widths(cx);
        let mut free = f32::from(self.content_width) - 180. - 24.;
        let mut take = |want: bool, width: f32| -> bool {
            if want && width <= free {
                free -= width;
                true
            } else {
                false
            }
        };
        let size = take(c.size, w[3]);
        let modified = take(c.modified, w[0]);
        let kind = take(c.kind, f32::from(self.type_col_w(cx)));
        let git = take(c.git && self.git.is_some(), w[5]);
        let tags = take(c.tags && !self.is_search(), w[4]);
        let sync = take(cloud, w[6]);
        let created = take(c.created && !self.is_search(), w[1]);
        let repo = self.git.is_some() && !self.is_search();
        let message = take(c.git_message && repo, w[7]);
        let date = take(c.git_date && repo, w[9]);
        let author = take(c.git_author && repo, w[8]);
        let hash = take(c.git_hash && repo, w[10]);
        Cols { modified, created, kind, size, tags, git, sync, commit: [message, author, date, hash] }
    }

    /// Width the visible Details columns leave unused (what [`Self::visible_cols`] still had free).
    fn spare_width(&self, cx: &App) -> f32 {
        let vis = self.visible_cols(cx);
        let w = self.col_widths(cx);
        let mut used = 0.;
        for (on, width) in [(vis.modified, w[0]), (vis.created, w[1]), (vis.size, w[3]), (vis.tags, w[4]), (vis.git, w[5]), (vis.sync, w[6])] {
            if on {
                used += width;
            }
        }
        if vis.kind {
            used += f32::from(self.type_col_w(cx));
        }
        for (i, on) in vis.commit.iter().enumerate() {
            if *on {
                used += w[7 + i];
            }
        }
        (f32::from(self.content_width) - 180. - 24. - used).max(0.)
    }

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let w = self.col_widths(cx);
        let search = self.is_search();
        // drag handle on a column's left edge: dragging left widens it (the name column gives way)
        let handle = |ix: usize, cx: &mut Context<Self>| {
            div()
                .id(("col-resize", ix))
                .absolute()
                .left(px(-4.))
                .top_0()
                .h_full()
                .w(px(8.))
                .cursor_col_resize()
                .child(div().mx_auto().w(px(1.)).h_full().bg(p.divider))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let cur = this.col_widths(cx)[ix];
                        this.col_drag = Some((ix, f32::from(ev.position.x), cur));
                    }),
                )
                .on_drag(ColumnResize, |_, _, _, cx| cx.new(|_| ColumnGhost))
        };
        let vis = self.visible_cols(cx);
        let (show_tags, show_git, show_sync) = (vis.tags, vis.git, vis.sync);
        // columns that don't sort: plain label + resize handle
        let plain = |id: &'static str, label: &'static str, width: f32| {
            h_flex()
                .id(id)
                .h_full()
                .px_2()
                .items_center()
                .w(px(width))
                .flex_shrink_0()
                .overflow_hidden()
                .text_color(p.text_secondary)
                .child(div().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
                .relative()
        };
        let col = |id: &'static str, label: &'static str, key: SortKey, w: Option<Pixels>, right: bool, cx: &mut Context<Self>| {
            let active = self.sort.key == key;
            let arrow = if self.sort.dir == SortDir::Asc { L::ChevronUp } else { L::ChevronDown };
            let mut d = h_flex()
                .id(id)
                .h_full()
                .px_2()
                .gap_1()
                .items_center()
                .rounded(theme::RADIUS_SM)
                .cursor_pointer()
                .hover(|s| s.bg(p.hover))
                .text_color(if active { p.text } else { p.text_secondary })
                .when(right, |d| d.justify_end())
                .overflow_hidden()
                .child(div().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
                .when(active, |d| d.child(Icon::new(arrow).size(px(12.)).flex_shrink_0().text_color(p.text_secondary)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_sort(key, cx)));
            d = match w {
                Some(w) => d.w(w).flex_shrink_0(),
                None => d.flex_1().min_w(px(120.)),
            };
            d
        };
        h_flex()
            .h(px(30.))
            .px(px(10.))
            .overflow_hidden()
            .text_xs()
            .border_b_1()
            .border_color(p.divider)
            .on_drag_move(cx.listener(|this, e: &DragMoveEvent<ColumnResize>, _, cx| {
                let Some((ix, sx, sw)) = this.col_drag else { return };
                let mut w = this.col_widths(cx);
                // no wider than the room left: a column that no longer fits is hidden, so
                // widening one would make it (or its neighbour) vanish under the pointer
                let most = (w[ix] + this.spare_width(cx)).clamp(w[ix].min(600.), 600.);
                w[ix] = (sw - (f32::from(e.event.position.x) - sx)).clamp(70., most.max(70.)).round();
                this.col_live = Some(w);
                this.schedule_col_save(cx);
                cx.notify();
            }))
            .child(col("h-name", t!("Ad"), SortKey::Name, None, false, cx))
            .children(show_tags.then(|| plain("h-tags", t!("Dosya etiketi"), w[4]).child(handle(4, cx))))
            .children(show_git.then(|| plain("h-git", t!("Git durumu"), w[5]).child(handle(5, cx))))
            .children(show_sync.then(|| plain("h-sync", t!("Eşitleme durumu"), w[6]).child(handle(6, cx))))
            .children(vis.commit[0].then(|| plain("h-gmsg", t!("Son commit mesajı"), w[7]).child(handle(7, cx))))
            .children(vis.commit[1].then(|| plain("h-gauth", t!("Commit yazarı"), w[8]).child(handle(8, cx))))
            .children(vis.commit[2].then(|| plain("h-gdate", t!("Commit tarihi"), w[9]).child(handle(9, cx))))
            .children(vis.commit[3].then(|| plain("h-ghash", t!("Commit hash"), w[10]).child(handle(10, cx))))
            .children(vis.modified.then(|| {
                col("h-mod", t!("Değiştirme tarihi"), SortKey::Modified, Some(px(w[0])), false, cx).relative().child(handle(0, cx))
            }))
            .children(vis.created.then(|| {
                col("h-created", t!("Oluşturma tarihi"), SortKey::Created, Some(px(w[1])), false, cx).relative().child(handle(1, cx))
            }))
            .children(vis.kind.then(|| {
                let c = col("h-type", if search { t!("Konum") } else { t!("Tür") }, SortKey::Type, Some(self.type_col_w(cx)), false, cx).relative();
                if search { c } else { c.child(handle(2, cx)) }
            }))
            .children(vis.size.then(|| col("h-size", t!("Boyut"), SortKey::Size, Some(px(w[3])), true, cx).relative().child(handle(3, cx))))
    }

    /// Writes dragged column widths to Settings once the drag pauses (a save per move would
    /// rewrite settings.toml dozens of times a second).
    fn schedule_col_save(&mut self, cx: &mut Context<Self>) {
        self.col_save_gen += 1;
        let generation = self.col_save_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
            let _ = this.update(cx, |v, cx| {
                if v.col_save_gen != generation {
                    return;
                }
                if let Some(w) = v.col_live.take() {
                    v.col_drag = None;
                    crate::settings::update(cx, |s| {
                        s.columns.widths = [w[0], w[1], w[2], w[3]].map(|x| x as u16);
                        s.columns.extra_widths = [w[4], w[5], w[6]].map(|x| x as u16);
                        s.columns.git_widths = [w[7], w[8], w[9], w[10]].map(|x| x as u16);
                    });
                }
            });
        })
        .detach();
    }

    fn render_rows(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = Palette::for_window(window);
        let drop_bg = p.selection;
        let drop_border = p.accent;
        let search_mode = self.is_search();
        let prefs = cx.global::<crate::settings::Settings>();
        let show_ext = prefs.show_extensions;
        let checkboxes = prefs.show_checkboxes;
        let hover_select = prefs.select_on_hover;
        // folders inside an archive have no size of their own to measure
        let measurable = !self.location().path().is_some_and(crate::archive_view::is_browsable_path);
        let folder_sizes = prefs.calculate_folder_sizes;
        let on_disk = prefs.folder_size_on_disk;
        let vis = self.visible_cols(cx);
        let show_date = vis.modified;
        let show_created = vis.created;
        let show_type = vis.kind;
        let show_size = vis.size;
        let type_w = self.type_col_w(cx);
        let cw = self.col_widths(cx);
        let grouped = self.grouped();
        let row_h = self.row_h();
        let (col_tags, col_git, col_sync) = (vis.tags, vis.git, vis.sync);
        let mut out = Vec::with_capacity(range.len());
        for r in range {
            let vi = if grouped {
                match self.rows.get(r).cloned() {
                    Some(Row::Item(vi)) => vi,
                    Some(Row::Header(label, n)) => {
                        out.push(
                            h_flex()
                                .id(("grp", r))
                                .w_full()
                                .h(row_h)
                                .px_2()
                                .pt_2()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text_secondary)
                                .child(crate::i18n::td(&label))
                                .child(div().text_color(p.text_tertiary).child(format!("{n}")))
                                .child(div().flex_1().h(px(1.)).bg(p.divider))
                                .into_any_element(),
                        );
                        continue;
                    }
                    None => break,
                }
            } else {
                r
            };
            let Some(&ei) = self.visible.get(vi) else { break };
            // Folder sizes: every visible one with Settings → "Calculate folder sizes", otherwise
            // only those clicked ("Boyutu hesapla"); measured in the background.
            let dir_path = (show_size && measurable && self.entries[ei as usize].is_dir()).then(|| self.path_of(ei)).flatten();
            let dir_size: Option<String> = dir_path.clone().and_then(|p| {
                if folder_sizes {
                    Some(self.folder_size(p, cx).text(on_disk))
                } else {
                    self.folder_sizes.get(&p).map(|s| s.text(on_disk))
                }
            });
            let calc_link = dir_path.filter(|_| dir_size.is_none());
            let e = &self.entries[ei as usize];
            let selected = self.selected.contains(&ei);
            let is_cursor = self.cursor == Some(vi);
            let cat = icons::category(e);
            let dim = e.is_hidden() || e.is_cloud_placeholder();
            let name: SharedString = shown_name(e, show_ext).into();
            let shell_icon = self
                .parent_of(ei)
                .map(|d| d.to_path_buf())
                .and_then(|d| self.images.update(cx, |c, cx| c.icon(&d, e, cx)));
            let git_state = self.git.as_ref().and_then(|g| rf_win::git::state_for_child(g, self.parent_of(ei)?, &e.name));
            let git_cell = col_git.then(|| {
                let (label, color) = match git_state {
                    Some(st) => (git_label(st), git_style(st).1),
                    None => ("", p.text_secondary),
                };
                div().w(px(cw[5])).flex_shrink_0().px_2().text_color(color).whitespace_nowrap().overflow_hidden().text_ellipsis().child(label)
            });
            let git_badge = (!col_git).then_some(()).and(self.git.as_ref()).and_then(|g| {
                let dir = self.parent_of(ei)?;
                let st = rf_win::git::state_for_child(g, dir, &e.name)?;
                let (letter, color) = git_style(st);
                Some(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .rounded(px(3.))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(letter),
                )
            });
            // commit columns: only for the current folder's own children
            let commit = vis
                .commit
                .iter()
                .any(|&b| b)
                .then_some(self.git_commits.as_ref())
                .flatten()
                .filter(|_| self.parent_of(ei) == self.location().path())
                .and_then(|m| m.get(&*e.name));
            let commit_cells: Vec<AnyElement> = (0..4)
                .filter(|&i| vis.commit[i])
                .map(|i| {
                    let text: String = match (i, commit) {
                        (_, None) => String::new(),
                        (0, Some(c)) => c.message.clone(),
                        (1, Some(c)) => c.author.clone(),
                        (2, Some(c)) => format::datetime(c.time_ms),
                        (_, Some(c)) => c.sha.clone(),
                    };
                    div()
                        .w(px(cw[7 + i]))
                        .flex_shrink_0()
                        .px_2()
                        .text_color(p.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(text)
                        .into_any_element()
                })
                .collect();
            let tag_cell = col_tags.then(|| {
                let store = &cx.global::<crate::tags::Tags>().0;
                let ids: Vec<u32> = self.path_of(ei).map(|path| store.tags_of(&path).to_vec()).unwrap_or_default();
                let mut cell = h_flex().w(px(cw[4])).flex_shrink_0().px_2().gap_1().items_center().overflow_hidden().text_color(p.text_secondary);
                for (i, id) in ids.iter().enumerate() {
                    if let Some(d) = store.def(*id) {
                        cell = cell.child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(crate::tags::color(d.color)));
                        if i == 0 {
                            cell = cell.child(div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(crate::i18n::td(&d.name)));
                        }
                    }
                }
                cell
            });
            let tag_dots = (!col_tags).then_some(()).and(self.path_of(ei)).and_then(|path| {
                let store = &cx.global::<crate::tags::Tags>().0;
                let ids = store.tags_of(&path);
                (!ids.is_empty()).then(|| {
                    let mut row = h_flex().gap_0p5().flex_shrink_0();
                    for id in ids {
                        if let Some(d) = store.def(*id) {
                            row = row.child(div().size(px(8.)).rounded_full().bg(crate::tags::color(d.color)));
                        }
                    }
                    row
                })
            });
            // OneDrive & co. (Files' sync status): online-only = cloud, always kept here = check
            let sync_cell = col_sync.then(|| {
                const PINNED: u32 = 0x80000;
                let (icon, color, label) = if e.is_cloud_placeholder() {
                    (Some(L::Cloud), p.accent, t!("Yalnızca çevrimiçi"))
                } else if e.attrs & PINNED != 0 {
                    (Some(L::CircleCheck), gpui_kit::rgb(0x2EA043).into(), t!("Her zaman bu cihazda"))
                } else {
                    (Some(L::CircleCheck), p.text_tertiary, t!("Bu cihazda kullanılabilir"))
                };
                h_flex()
                    .w(px(cw[6]))
                    .flex_shrink_0()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .overflow_hidden()
                    .text_color(p.text_secondary)
                    .children(icon.map(|ic| Icon::new(ic).size(px(14.)).text_color(color)))
                    .child(div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
            });
            let cloud_badge = if col_sync { None } else {
                const PINNED: u32 = 0x80000;
                let (icon, color, tip) = if e.is_cloud_placeholder() {
                    (Some(L::Cloud), p.accent, t!("Yalnızca çevrimiçi"))
                } else if e.attrs & PINNED != 0 {
                    (Some(L::CircleCheck), gpui_kit::rgb(0x2EA043).into(), t!("Her zaman bu cihazda"))
                } else {
                    (None, p.text, "")
                };
                icon.map(|ic| {
                    div()
                        .id(("cloud", vi))
                        .flex_shrink_0()
                        .px_1()
                        .child(Icon::new(ic).size(px(14.)).text_color(color))
                        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(w, cx))
                })
            };
            let rename_input = self.renaming.as_ref().filter(|(i, _)| *i == ei).map(|(_, inp)| inp.clone());
            let name_el: AnyElement = match rename_input {
                // the box grows to the left (border + padding) so the text keeps its place
                Some(inp) => div()
                    .flex_1()
                    .h(px(24.))
                    .ml(px(-RENAME_INSET))
                    .px(px(RENAME_INSET - 1.))
                    .rounded(px(4.))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.accent)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(gpui_kit::component::input::Input::new(&inp).appearance(false).small().px_0())
                    .into_any_element(),
                None => div()
                    .min_w_0()
                    .flex_shrink(1.)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(dim, |d| d.opacity(0.6))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, _| this.name_hit = true))
                    .child(name)
                    .into_any_element(),
            };
            let row = h_flex()
                .id(("row", vi))
                .group("rowgrp")
                .h(row_h)
                .w_full()
                .px(px(4.))
                .rounded(theme::RADIUS_SM)
                .items_center()
                .text_sm()
                .relative()
                .when(selected, |d| d.bg(p.selection).child(theme::selection_pill(&p)))
                .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
                .when(is_cursor && selected && self.selected.len() > 1, |d| {
                    d.border_1().border_color(p.selection_border)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| this.on_row_mouse_down(vi, ev, window, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| this.on_row_right_down(vi, window, cx)),
                )
                .when(e.is_dir(), |d| {
                    d.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_bg).border_1().border_color(drop_border))
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                            if let Some(dest) = this.drop_target_row(ei) {
                                this.drop_into(dest, paths, cx);
                            }
                        }))
                })
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        let Some(&ei) = this.visible.get(vi) else { return };
                        if this.entries[ei as usize].is_dir() {
                            if let Some(p) = this.path_of(ei) {
                                cx.emit(FolderEvent::OpenInNewTab(Location::Path(p)));
                            }
                        }
                    }),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w(px(120.))
                        .px_2()
                        .gap_2()
                        .items_center()
                        .overflow_hidden()
                        // Settings → "Show checkboxes when selecting items"
                        .when(checkboxes, |d| {
                            d.child(
                                div()
                                    .id(("chk", vi))
                                    .size(px(16.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(4.))
                                    .border_1()
                                    .when(selected, |d| d.bg(p.accent).border_color(p.accent))
                                    .when(!selected, |d| d.border_color(p.text_tertiary).invisible().group_hover("rowgrp", |s| s.visible()))
                                    .when(selected, |d| d.child(Icon::new(L::Check).size(px(11.)).text_color(p.accent_text)))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                            cx.stop_propagation();
                                            let Some(&ei) = this.visible.get(vi) else { return };
                                            if !this.selected.remove(&ei) {
                                                this.selected.insert(ei);
                                            }
                                            this.cursor = Some(vi);
                                            this.anchor = Some(vi);
                                            cx.notify();
                                        }),
                                    ),
                            )
                        })
                        .child(match &shell_icon {
                            Some(im) => img(im.clone()).size(px(16.)).flex_shrink_0().into_any_element(),
                            None => Icon::new(icons::icon_for(cat))
                                .size(px(16.))
                                .flex_shrink_0()
                                .text_color(theme::kind_color(cat, p.dark))
                                .into_any_element(),
                        })
                        .child(name_el)
                        // encrypted: an archive member that needs a password, or an EFS file
                        .children((e.attrs & rf_core::attrs::ENCRYPTED != 0).then(|| Icon::new(L::Lock).size(px(12.)).flex_shrink_0().text_color(p.text_secondary)))
                        .children(tag_dots)
                        .children(git_badge)
                        .child(div().flex_1())
                        .children(cloud_badge),
                )
                .children(tag_cell)
                .children(git_cell)
                .children(sync_cell)
                .children(commit_cells)
                .children(show_date.then(|| {
                    div()
                        .w(px(cw[0]))
                        .flex_shrink_0()
                        .px_2()
                        .text_color(p.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(format::datetime(e.modified_ms))
                }))
                .children(show_created.then(|| {
                    div()
                        .w(px(cw[1]))
                        .flex_shrink_0()
                        .px_2()
                        .text_color(p.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(format::datetime(e.created_ms))
                }))
                .when(show_type, |row| row.child(
                    div()
                        .w(type_w)
                        .flex_shrink_0()
                        .px_2()
                        .text_color(p.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(if search_mode {
                            self.parent_of(ei).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
                        } else {
                            icons::type_label(e)
                        }),
                ))
                .when(show_size, |row| {
                    let cell = div().w(px(cw[3])).flex_shrink_0().px_2().flex().justify_end().whitespace_nowrap().overflow_hidden();
                    let cell = match calc_link {
                        // "Boyutu hesapla": measures just this folder (doesn't select the row)
                        Some(dir) => cell.child(
                            div()
                                .id(("calc-size", vi))
                                .text_xs()
                                .text_color(p.text_tertiary)
                                .cursor_pointer()
                                .hover(|s| s.text_color(p.accent).underline())
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(t!("Boyutu hesapla"))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        this.folder_size(dir.clone(), cx);
                                        cx.notify();
                                    }),
                                ),
                        ),
                        None => {
                            let text = if !e.is_dir() { format::size(e.size) } else { dir_size.clone().unwrap_or_default() };
                            cell.text_color(p.text_secondary).child(text)
                        }
                    };
                    row.child(cell)
                })
                // Settings → "Select files and folders on hover"
                .when(hover_select, |row| {
                    row.on_hover(cx.listener(move |this, on: &bool, _, cx| {
                        if *on && this.drag_origin.is_none() && this.marquee.is_none() && this.renaming.is_none() {
                            this.select_only(vi);
                            cx.notify();
                        }
                    }))
                });
            out.push(row.into_any_element());
        }
        out
    }

    fn render_details(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.visible.len();
        // Details and Grid get a scrollbar over the list (Columns has one list per column).
        let mut list_body = false;
        let body: AnyElement = if let Some(err) = &self.error {
            empty_state(p, L::TriangleAlert, crate::i18n::td(err), t!("Klasör açılamadı")).into_any_element()
        } else if count == 0 && self.loading && self.load_started.elapsed() >= SLOW_LOAD {
            empty_state(p, L::LoaderCircle, t!("Yükleniyor…").to_string(), "").into_any_element()
        } else if count == 0 && !self.loading {
            let msg = if self.filter.query.is_empty() { t!("Bu klasör boş") } else { t!("Eşleşen öğe yok") };
            empty_state(p, L::FolderOpen, msg.to_string(), "").into_any_element()
        } else if self.view_mode == ViewMode::Columns {
            self.render_columns(p, cx)
        } else if self.view_mode == ViewMode::Grid {
            let avail = self.content_width - px(12.);
            let cols = ((avail / (self.grid_tile_w() + GRID_GAP)).floor() as usize).max(1);
            self.grid_cols = cols;
            let rows = count.div_ceil(cols);
            list_body = true;
            uniform_list("grid", rows, cx.processor(|this, range, window, cx| this.render_grid_rows(range, window, cx)))
                .track_scroll(&self.scroll)
                .flex_1()
                .py_2()
                .px(px(6.))
                .into_any_element()
        } else {
            let n_rows = if self.grouped() { self.rows.len() } else { count };
            list_body = true;
            uniform_list("rows", n_rows, cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)))
                .track_scroll(&self.scroll)
                .flex_1()
                .py_1()
                .px(px(6.))
                .into_any_element()
        };
        let this = cx.entity();
        let width_probe = canvas(
            move |bounds, window, cx| {
                let changed = this.update(cx, |v, cx| {
                    let changed = v.content_width != bounds.size.width;
                    if changed {
                        v.content_width = bounds.size.width;
                        cx.notify();
                    }
                    changed
                });
                // Width-dependent layout (columns, grid) was built with the old width: redraw.
                if changed {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let header = (self.view_mode == ViewMode::Details).then(|| self.render_header(p, cx));
        v_flex()
            .size_full()
            .relative()
            .child(width_probe)
            .children(header)
            .child(
                div()
                    .id("rows-bg")
                    .flex_1()
                    .flex()
                    .flex_col()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            // Settings → "Double-click an empty area to go up"
                            if ev.click_count == 2 && cx.global::<crate::settings::Settings>().double_click_up {
                                this.up(cx);
                                return;
                            }
                            if !(ev.modifiers.control || ev.modifiers.shift) && !this.selected.is_empty() {
                                this.selected.clear();
                                cx.notify();
                            }
                            this.begin_marquee(ev);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            this.selected.clear();
                            cx.notify();
                            this.show_context_menu(true, window, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                        if this.marquee.is_some() {
                            if ev.pressed_button == Some(MouseButton::Left) {
                                this.update_marquee(ev.position, cx);
                            } else {
                                this.marquee = None;
                                cx.notify();
                            }
                            return;
                        }
                        this.on_drag_motion(ev, window, cx)
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| {
                            this.drag_origin = None;
                            if this.marquee.take().is_some() {
                                cx.notify();
                            }
                        }),
                    )
                    .relative()
                    .children(self.render_marquee(p))
                    .on_drag_move::<ExternalPaths>(cx.listener(|this, ev: &DragMoveEvent<ExternalPaths>, _, cx| {
                        if let Some(dir) = this.current_dir() {
                            let paths: Vec<PathBuf> = ev.drag(cx).paths().to_vec();
                            let t = rf_win::dnd::default_transfer(&paths, &dir, false, false);
                            gpui_windows::dnd_hint::set(match t {
                                rf_win::fileops::Transfer::Move => gpui_windows::dnd_hint::MOVE,
                                rf_win::fileops::Transfer::Copy => gpui_windows::dnd_hint::COPY,
                            });
                        }
                    }))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        if let Some(dir) = this.current_dir() {
                            this.drop_into(dir, paths, cx);
                        }
                    }))
                    .child(body)
                    // last child: painted over the list, so its clicks and drags come first. Shown
                    // while scrolling and fading out after, like File Explorer's on Windows 11,
                    // so it doesn't sit over the right-aligned Size column
                    .when(list_body && cx.global::<crate::settings::Settings>().show_scrollbar, |d| {
                        d.child(Scrollbar::vertical(&self.scroll).id("list-scrollbar").mode(ScrollbarMode::Scrolling))
                    }),
            )
            .into_any_element()
    }

    fn render_home(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let places = self.places.read(cx);
        let pinned = places.pinned.clone();
        let drives = places.drives.clone();
        let recent = places.recent.clone();
        let devices = places.devices.clone();
        let card = |id: ElementId| {
            v_flex()
                .id(id)
                .p_3()
                .gap_2()
                .min_w_0()
                .rounded(theme::RADIUS)
                .bg(p.tile_bg)
                .border_1()
                .border_color(p.border)
                .cursor_pointer()
                .hover(|s| s.bg(p.tile_hover))
        };
        let section = |title: &'static str| div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title);

        // equal-width columns: a card alone on the last row is as wide as the others
        let avail = f32::from(self.content_width) - 48.; // p_6 on both sides
        let mut pinned_grid = div().grid().grid_cols(tile_cols(avail, 140., 220., 12.)).gap_3();
        for (i, f) in pinned.into_iter().enumerate() {
            let path = f.path.clone();
            pinned_grid = pinned_grid.child(
                card(("pin", i).into())
                    .child(Icon::new(f.icon).size(px(28.)).text_color(p.accent))
                    .child(div().text_sm().child(crate::i18n::td(&f.title)))
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(Location::Path(path.clone()), cx))),
            );
        }

        let mut drive_grid = div().grid().grid_cols(tile_cols(avail, 240., 340., 12.)).gap_3();
        for (i, d) in drives.into_iter().enumerate() {
            let root = d.root.clone();
            let usage_root = d.root.clone();
            let frac = d.used_fraction();
            let bar_color = if frac > 0.9 { p.danger } else { p.accent };
            drive_grid = drive_grid.child(
                card(("drive", i).into())
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(Icon::new(places::drive_icon(&d)).size(px(28.)).text_color(p.text_secondary))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(div().text_sm().child(crate::places::drive_name(&d)))
                                    .child(
                                        div()
                                            .h(px(6.))
                                            .w_full()
                                            .rounded_full()
                                            .bg(p.pressed)
                                            .child(div().h_full().rounded_full().bg(bar_color).w(relative(frac))),
                                    )
                                    .child(div().text_xs().text_color(p.text_secondary).child(if d.total > 0 {
                                        format!("{} {} / {}", format::size(d.free), t!("boş"), format::size(d.total))
                                    } else {
                                        String::new()
                                    })),
                            )
                            // where the space goes, as its own tab (the card itself opens the drive)
                            .child(
                                div()
                                    .id(("drive-usage", i))
                                    .size(px(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(theme::RADIUS_SM)
                                    .hover(|s| s.bg(p.hover))
                                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new(t!("Disk kullanımını göster")).build(w, cx))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(FolderEvent::OpenInNewTab(Location::DiskUsage(usage_root.clone())))
                                    }))
                                    .child(Icon::new(L::ChartPie).size(px(16.)).text_color(p.text_secondary)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(Location::Path(root.clone()), cx))),
            );
        }

        // phones and cameras (not drives): none connected = no group at all
        let has_devices = !devices.is_empty();
        let mut device_grid = div().grid().grid_cols(tile_cols(avail, 240., 340., 12.)).gap_3();
        for (i, d) in devices.into_iter().enumerate() {
            let loc = Location::Shell { parsing: d.parsing.clone(), name: d.name.clone() };
            device_grid = device_grid.child(
                card(("device", i).into())
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(Icon::new(L::Usb).size(px(28.)).text_color(p.text_secondary))
                            .child(div().flex_1().min_w_0().text_sm().overflow_hidden().text_ellipsis().whitespace_nowrap().child(d.name)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(loc.clone(), cx))),
            );
        }

        let has_recent = !recent.is_empty();
        let mut recent_list = v_flex().gap_0p5().max_w(px(900.));
        for (i, r) in recent.into_iter().enumerate() {
            let e = rf_core::Entry::new(&r.name, if r.is_dir { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File }, 0, 0, 0, 0);
            let cat = icons::category(&e);
            let parent = r.target.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let target = r.target.clone();
            let is_dir = r.is_dir;
            // folders under their Explorer name (Downloads → İndirilenler)
            let shown: String = if is_dir { rf_win::localized::folder_label(&target, None).map_or_else(|| r.name.clone(), |l| l.to_string()) } else { r.name.clone() };
            recent_list = recent_list.child(
                h_flex()
                    .id(("recent", i))
                    .h(theme::ROW_H)
                    .px_2()
                    .gap_3()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .text_sm()
                    .hover(|s| s.bg(p.hover))
                    .cursor_pointer()
                    .child(Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, p.dark)))
                    .child(div().w(px(260.)).flex_shrink_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(shown))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(p.text_tertiary)
                            .child(parent),
                    )
                    .child(div().w(px(130.)).flex_shrink_0().text_color(p.text_secondary).child(format::datetime(r.used_ms)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if is_dir {
                            this.navigate(Location::Path(target.clone()), cx);
                        } else {
                            open_in_background(target.clone(), cx);
                        }
                    })),
            );
        }

        // the Home page measures its own width (the list's probe isn't drawn here)
        let this = cx.entity();
        let width_probe = canvas(
            move |bounds, window, cx| {
                let changed = this.update(cx, |v, cx| {
                    let changed = v.content_width != bounds.size.width;
                    if changed {
                        v.content_width = bounds.size.width;
                        cx.notify();
                    }
                    changed
                });
                if changed {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div()
            .id("home")
            .size_full()
            .relative()
            .overflow_y_scroll()
            .child(width_probe)
            .child(
                v_flex()
                    .p_6()
                    .gap_4()
                    .child(section(t!("Hızlı Erişim")))
                    .child(pinned_grid)
                    .child(div().h_2())
                    .child(section(t!("Sürücüler")))
                    .child(drive_grid)
                    .when(has_devices, |d| d.child(div().h_2()).child(section(t!("Cihazlar"))).child(device_grid))
                    .when(has_recent, |d| d.child(div().h_2()).child(section(t!("Son kullanılanlar"))))
                    .child(recent_list),
            )
            .into_any_element()
    }
}

/// Columns for a row of equal cards: as many as fit at `min` width, but enough that none is
/// wider than `max` (all columns share the width evenly).
fn tile_cols(avail: f32, min: f32, max: f32, gap: f32) -> u16 {
    let fit = ((avail + gap) / (min + gap)).floor().max(1.);
    let need = ((avail + gap) / (max + gap)).ceil().max(1.);
    need.min(fit) as u16
}

#[cfg(test)]
mod tile_tests {
    use super::tile_cols;

    #[test]
    fn cards_stay_between_min_and_max_and_equal() {
        for avail in [300f32, 600., 893., 1000., 1400., 2000.] {
            for (min, max) in [(140f32, 220f32), (240., 340.)] {
                let n = tile_cols(avail, min, max, 12.) as f32;
                let w = (avail - 12. * (n - 1.)) / n;
                assert!(w <= max + 0.01 || n == 1., "{avail}: {n} cols of {w}");
                assert!(w >= min - 0.01 || n == 1., "{avail}: {n} cols of {w}");
            }
        }
        assert_eq!(tile_cols(100., 140., 220., 12.), 1); // narrow: one column
    }

    /// Filtering the cached sort order gives exactly what sorting the filtered entries gives.
    #[test]
    fn filtering_the_sorted_order_matches_a_full_sort() {
        use super::{GroupBy, compute_view, view_from_sorted};
        use rf_core::{Entry, EntryKind, entry::Filter, sort::{SortKey, SortSpec}};
        let names = ["Rapor 10.pdf", "rapor 2.pdf", "Çizim.png", "notlar.txt", ".gizli", "Proje", "ışık.svg", "RAPOR eski.docx", "a", "b10", "b9"];
        let entries: Vec<Entry> = names
            .iter()
            .enumerate()
            .map(|(i, n)| Entry::new(n, if *n == "Proje" { EntryKind::Dir } else { EntryKind::File }, (i as u64 * 37) % 11, i as i64, 0, 0))
            .collect();
        for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
            let sort = SortSpec { key, ..Default::default() };
            let base = compute_view(&entries, &Filter::default(), sort, GroupBy::None);
            for q in ["", "rapor", "R", "ı", "zzz"] {
                for dot_hidden in [false, true] {
                    let f = Filter { query: q.into(), dot_hidden, ..Default::default() };
                    let full = compute_view(&entries, &f, sort, GroupBy::None);
                    let fast = view_from_sorted(&entries, base.sorted.clone(), &f, sort, GroupBy::None);
                    assert_eq!(full.visible, fast.visible, "{key:?} {q:?}");
                }
            }
        }
    }
}

/// Files' "Git status" column text.
fn git_label(s: rf_win::git::FileState) -> &'static str {
    use rf_win::git::FileState::*;
    match s {
        Modified => t!("Değiştirildi"),
        Added => t!("Eklendi"),
        Deleted => t!("Silindi"),
        Renamed => t!("Yeniden adlandırıldı"),
        Untracked => t!("İzlenmiyor"),
        Ignored => t!("Yok sayıldı"),
        Conflicted => t!("Çakışma"),
    }
}

fn git_style(s: rf_win::git::FileState) -> (&'static str, Hsla) {
    use rf_win::git::FileState::*;
    let c = |x: u32| -> Hsla { rgb(x).into() };
    match s {
        Modified => ("M", c(0xFF9F0A)),
        Added => ("A", c(0x30D158)),
        Deleted => ("D", c(0xFF453A)),
        Renamed => ("R", c(0x0A84FF)),
        Untracked => ("U", c(0x8E8E93)),
        Ignored => ("I", c(0x636366)),
        Conflicted => ("!", c(0xFF453A)),
    }
}

/// File name as displayed (extension hidden when the setting says so).
fn shown_name(e: &Entry, show_ext: bool) -> String {
    if show_ext || e.is_dir() || e.ext.is_empty() {
        e.shown().to_string()
    } else {
        e.name[..e.stem_len()].to_string()
    }
}

const COL_W: Pixels = px(250.);

/// A read-only column (ancestor or next folder) in the columns view.
fn static_column(
    id: ElementId,
    dir: PathBuf,
    list: Option<Arc<Vec<Entry>>>,
    highlight: Option<String>,
    scroll: Option<UniformListScrollHandle>,
    p: &Palette,
    view: Entity<FolderView>,
) -> impl IntoElement {
    let body: AnyElement = match list {
        None => div().p_3().text_xs().text_color(p.text_tertiary).child(t!("Yükleniyor…")).into_any_element(),
        Some(list) if list.is_empty() => div().p_3().text_xs().text_color(p.text_tertiary).child(t!("Boş")).into_any_element(),
        Some(list) => {
            let n = list.len();
            let pal = *p;
            uniform_list(id.clone(), n, move |range: std::ops::Range<usize>, _, _| {
                range
                    .map(|i| {
                        let e = &list[i];
                        let on = highlight.as_deref() == Some(&*e.name);
                        let cat = icons::category(e);
                        let target = dir.join(&*e.name);
                        let is_dir = e.is_dir();
                        let view = view.clone();
                        h_flex()
                            .id(("scol", i))
                            .w_full()
                            .h(px(28.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .rounded(theme::RADIUS_SM)
                            .text_sm()
                            .when(on, |d| d.bg(pal.hover).font_weight(FontWeight::MEDIUM))
                            .when(!on, |d| d.hover(|s| s.bg(pal.hover)).text_color(pal.text_secondary))
                            .child(Icon::new(icons::icon_for(cat)).size(px(16.)).text_color(theme::kind_color(cat, pal.dark)))
                            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(SharedString::from(e.name.to_string())))
                            .when(is_dir, |d| d.child(Icon::new(L::ChevronRight).size(px(12.)).text_color(pal.text_tertiary)))
                            .on_click(move |_, _, cx| {
                                if is_dir {
                                    view.update(cx, |v, cx| v.navigate(Location::Path(target.clone()), cx));
                                } else {
                                    open_in_background(target.clone(), cx);
                                }
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .when_some(scroll, |l, h| l.track_scroll(&h))
            .size_full()
            .p_1()
            .into_any_element()
        }
    };
    div().w(COL_W).flex_shrink_0().h_full().border_r_1().border_color(p.divider).child(body)
}

const GRID_GAP: Pixels = px(4.);
/// Grid sizes (Ctrl+= / Ctrl+-): tile width, row height, thumbnail pixels.
const GRID_SIZES: [(f32, f32, u32); 5] = [(76., 94., 64), (92., 112., 80), (124., 150., 128), (172., 204., 192), (236., 276., 272)];
const DEFAULT_GRID_SIZE: u8 = 2;
/// Details row heights for the "Size" slider (30 = the classic one).
const ROW_SIZES: [f32; 5] = [26., 30., 36., 44., 52.];
const DEFAULT_ROW_SIZE: u8 = 1;

const SLOW_LOAD: std::time::Duration = std::time::Duration::from_millis(400);

fn empty_state(p: &Palette, icon: L, title: String, sub: &'static str) -> impl IntoElement {
    v_flex()
        .flex_1()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(Icon::new(icon).size(px(40.)).text_color(p.text_tertiary))
        .child(div().text_color(p.text_secondary).child(title))
        .when(!sub.is_empty(), |d| d.child(div().text_xs().text_color(p.text_tertiary).child(sub)))
}

/// Inline rename: how far the edit box reaches left of the name (its border plus padding), so
/// the text inside starts exactly where the name was drawn.
const RENAME_INSET: f32 = 5.;

pub(crate) fn open_in_background(path: PathBuf, cx: &mut App) {
    if !rf_win::ftp::is_ftp(&path) && !path.exists() && crate::archive_view::split(&path).is_some() {
        // an item inside an archive: extract just it (Status Center), open the real file
        crate::archive_view::open_items(cx, vec![path]);
        return;
    }
    std::thread::spawn(move || {
        // FTP: download to a temp copy, then open that
        let path = if rf_win::ftp::is_ftp(&path) {
            let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let dir = std::env::temp_dir().join("smoothyfiles-ftp");
            let _ = std::fs::create_dir_all(&dir);
            let local = dir.join(name);
            if let Err(e) = rf_win::ftp::download(&path.to_string_lossy(), &local) {
                log::warn!("ftp download {path:?}: {e}");
                return;
            }
            local
        } else {
            path
        };
        if let Err(e) = rf_win::shell::open(&path) {
            log::warn!("open {path:?}: {e}");
        }
    });
}

impl Render for FolderView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.pending_rename) {
            // Create the editor now, but focus it on the next frame: a focus target that is not
            // part of the frame being drawn gets dropped by GPUI.
            self.begin_rename(window, cx);
            cx.on_next_frame(window, |this, window, cx| {
                if let Some((_, inp)) = this.renaming.clone() {
                    inp.update(cx, |s, cx| s.focus(window, cx));
                }
            });
        }
        let p = Palette::for_window(window);
        let content = match self.location() {
            Location::Home => self.render_home(&p, cx),
            Location::Settings => crate::settings_view::render(&p, cx),
            Location::DiskUsage(root) => {
                let root = root.clone();
                let dv = match &self.disk_usage {
                    Some(dv) if dv.read(cx).root() == root => dv.clone(),
                    _ => {
                        let dv = cx.new(|cx| crate::disk_usage::DiskUsageView::new(root, cx));
                        cx.subscribe(&dv, |this, _, ev: &crate::disk_usage::DiskEvent, cx| match ev {
                            crate::disk_usage::DiskEvent::ShowInFolder(path) => {
                                if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                                    this.set_reselect(name.to_string_lossy().into_owned());
                                    this.navigate(Location::Path(parent.to_path_buf()), cx);
                                }
                            }
                        })
                        .detach();
                        self.disk_usage = Some(dv.clone());
                        dv
                    }
                };
                div().size_full().child(dv).into_any_element()
            }
            Location::RecycleBin => {
                let rv = match &self.recycle {
                    Some(rv) => rv.clone(),
                    None => {
                        let rv = cx.new(crate::recycle_view::RecycleView::new);
                        self.recycle = Some(rv.clone());
                        rv
                    }
                };
                div().size_full().child(rv).into_any_element()
            }
            _ => self.render_details(&p, cx),
        };
        div()
            .id("folder-view")
            .key_context(FOLDER_CTX)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_cursor(this.row_step(), false, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.move_cursor(-this.row_step(), false, cx)))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.move_cursor(this.row_step(), true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPrev, _, cx| this.move_cursor(-this.row_step(), true, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| match this.view_mode {
                ViewMode::Grid => this.move_cursor(-1, false, cx),
                ViewMode::Columns => this.up(cx),
                ViewMode::Details => {}
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| match this.view_mode {
                ViewMode::Grid => this.move_cursor(1, false, cx),
                ViewMode::Columns => {
                    if let Some(ei) = this.cursor_entry().filter(|&ei| this.entries[ei as usize].is_dir()) {
                        this.open_entry(ei, cx);
                    }
                }
                ViewMode::Details => {}
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_cursor(20, false, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_cursor(-20, false, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor(isize::MIN / 2, false, cx)))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor(isize::MAX / 2, false, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.selected = this.visible.iter().copied().collect();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &InvertSelection, _, cx| {
                let next: HashSet<u32> = this.visible.iter().copied().filter(|i| !this.selected.contains(i)).collect();
                this.selected = next;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSelection, _, cx| {
                if let Some(ei) = this.cursor.and_then(|c| this.visible.get(c)).copied() {
                    if !this.selected.remove(&ei) {
                        this.selected.insert(ei);
                    }
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                if QUICK_LOOK_OPEN.load(Ordering::Relaxed) {
                    cx.emit(FolderEvent::QuickLook);
                    return;
                }
                if this.renaming.is_some() {
                    this.cancel_rename(window, cx);
                    return;
                }
                this.selected.clear();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                if this.renaming.is_some() {
                    this.commit_rename(window, cx)
                } else {
                    this.open_selected(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Properties, _, cx| this.properties(cx)))
            .on_action(cx.listener(|this, _: &OpenWith, _, _| this.open_with()))
            .on_action(cx.listener(|this, _: &CopyItems, window, cx| this.copy_to_clipboard(rf_win::fileops::Transfer::Copy, window, cx)))
            .on_action(cx.listener(|this, _: &CutItems, window, cx| this.copy_to_clipboard(rf_win::fileops::Transfer::Move, window, cx)))
            .on_action(cx.listener(|this, _: &PasteItems, window, cx| this.paste(window, cx)))
            .on_action(cx.listener(|this, _: &PasteIntoSelected, window, cx| this.paste_into_selected(window, cx)))
            .on_action(cx.listener(|this, _: &PasteShortcut, window, cx| this.paste_shortcuts(window, cx)))
            .on_action(cx.listener(|this, _: &DeleteItems, w, cx| this.delete_selected(false, w, cx)))
            .on_action(cx.listener(|this, _: &DeletePermanently, w, cx| this.delete_selected(true, w, cx)))
            .on_action(cx.listener(|this, _: &RenameItem, window, cx| {
                if this.selected.len() > 1 {
                    cx.emit(FolderEvent::BulkRename(this.selection_paths()));
                } else {
                    this.begin_rename(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &NewFolder, _, cx| this.create_new(true, cx)))
            .on_action(cx.listener(|this, _: &NewTextFile, _, cx| this.create_new(false, cx)))
            .on_action(cx.listener(|this, _: &CopyPath, _, cx| this.copy_path(cx)))
            .on_action(cx.listener(|this, _: &CopyPathQuoted, _, cx| this.copy_path_with(true, cx)))
            .on_action(cx.listener(|this, _: &RotateRight, window, cx| this.rotate_selection(true, window, cx)))
            .on_action(cx.listener(|this, _: &RotateLeft, window, cx| this.rotate_selection(false, window, cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                if this.renaming.is_none() {
                    crate::undo::redo(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1, cx)))
            .on_action(cx.listener(|this, _: &AddToShelf, _, cx| {
                let paths = this.selection_paths();
                if !paths.is_empty() {
                    cx.emit(FolderEvent::AddToShelf(paths));
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                if this.renaming.is_none() {
                    crate::undo::undo(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ShowContextMenu, window, cx| {
                let bg = this.selected.is_empty();
                this.show_context_menu(bg, window, cx)
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_typeahead(ev, cx)))
            .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| this.on_key_up(ev, cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.back(cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.forward(cx)),
            )
            .child(content)
    }
}

#[allow(dead_code)]
fn _assert_category_used(_: Category) {}

/// `SMOOTHYFILES_TRACE_LIVE=1`: log listing and live-update timings (benchmarks).
fn trace_live() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SMOOTHYFILES_TRACE_LIVE").is_some())
}

#[cfg(test)]
mod known_kind_tests {
    use super::{KnownKind, known_kind};
    use rf_win::known::KnownFolder;

    #[test]
    fn pictures_and_downloads_are_recognised() {
        let (Some(pics), Some(dl)) = (KnownFolder::Pictures.path(), KnownFolder::Downloads.path()) else { return };
        assert_eq!(known_kind(&pics), Some(KnownKind::Pictures));
        assert_eq!(known_kind(&pics.join("Screenshots")), Some(KnownKind::Pictures));
        assert_eq!(known_kind(&dl), Some(KnownKind::Downloads));
        // only Downloads itself gets date groups, not the folders inside it
        assert_eq!(known_kind(&dl.join("project")), None);
        // a sibling whose name starts the same is not inside Pictures
        let sibling = std::path::PathBuf::from(format!("{} old", pics.display()));
        assert_eq!(known_kind(&sibling), None);
    }
}

/// Name-index hits to rows: each path is looked up (size, dates, attributes) on 8 threads, 2 000
/// at a time, and sent in order; paths gone since they were indexed are left out. Returns how
/// many were sent.
fn send_index_hits(paths: &[PathBuf], tx: &mpsc::UnboundedSender<Msg>, cancel: &AtomicBool) -> usize {
    use std::os::windows::fs::MetadataExt;
    let ms = |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
    let row = |p: &PathBuf| -> Option<(Entry, Arc<std::path::Path>)> {
        let m = std::fs::symlink_metadata(p).ok()?;
        let kind = if m.is_dir() { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File };
        let name = p.file_name()?.to_string_lossy().into_owned();
        let parent: Arc<std::path::Path> = p.parent().unwrap_or(p).into();
        Some((Entry::new(&name, kind, if m.is_dir() { 0 } else { m.len() }, ms(m.modified()), ms(m.created()), m.file_attributes()), parent))
    };
    let mut sent = 0usize;
    for batch in paths.chunks(2000) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let per = batch.len().div_ceil(8).max(1);
        let parts: Vec<Vec<(Entry, Arc<std::path::Path>)>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch.chunks(per).map(|c| s.spawn(move || c.iter().filter_map(row).collect::<Vec<_>>())).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        let (es, ps): (Vec<Entry>, Vec<Arc<std::path::Path>>) = parts.into_iter().flatten().unzip();
        sent += es.len();
        let _ = tx.unbounded_send(Msg::Hits(es, ps));
    }
    sent
}
