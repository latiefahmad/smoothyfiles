//! Persistent user settings (%APPDATA%\smoothyfiles\settings.toml) as a GPUI global.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::i18n::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DefaultView {
    Details,
    Grid,
    Columns,
    /// Picks per folder, like Explorer's folder types: mostly pictures/videos → grid, Pictures
    /// and Videos → grid, Downloads → newest first in date groups, otherwise details.
    #[default]
    Adaptive,
}

/// Checking copies after they are written (our engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CopyVerify {
    #[default]
    Off,
    /// Size and modification time.
    Quick,
    /// Every byte, read back from the disk.
    Full,
}

impl CopyVerify {
    pub fn engine(self) -> rf_win::fastcopy::Verify {
        match self {
            CopyVerify::Off => rf_win::fastcopy::Verify::Off,
            CopyVerify::Quick => rf_win::fastcopy::Verify::Quick,
            CopyVerify::Full => rf_win::fastcopy::Verify::Full,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatePref {
    /// "5 dakika önce" for recent items.
    #[default]
    Application,
    System,
    Universal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StartupPref {
    /// Reopen last session's tabs.
    #[default]
    Continue,
    /// One Home tab.
    NewTab,
    /// The folders in `startup_pages`.
    Pages,
}

/// Settings → Appearance → Animations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AnimPref {
    /// Follow Windows (Settings → Accessibility → Visual effects → Animation effects).
    #[default]
    System,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackdropPref {
    /// Default, like Explorer: a quiet desktop tint.
    #[default]
    Mica,
    /// Files' default: a more colorful variant of Mica.
    MicaAlt,
    Acrylic,
    Solid,
}

/// Hızlı Bakış penceresinin arka planı.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LookBackdrop {
    #[default]
    Mica,
    Acrylic,
    Tabbed,
    /// Düz renk (tema rengi ya da `look_backdrop_color`).
    None,
}

/// Hızlı Bakış resim görüntüleyicisinin zemini.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LookImageTheme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeletePref {
    Always,
    /// Only for permanent deletes (the Recycle Bin can undo the rest) — Explorer's behaviour.
    #[default]
    PermanentOnly,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClickPref {
    #[default]
    Never,
    Folders,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IdePref {
    #[default]
    GitRepos,
    Always,
    Never,
}

/// Which of our own entries the context menu shows (Settings → General → Context menu).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MenuPrefs {
    pub open_in_new_tab: bool,
    pub open_in_new_window: bool,
    pub copy_path: bool,
    pub create_shortcut: bool,
    pub pin: bool,
    pub compress: bool,
    pub tags: bool,
    pub shelf: bool,
    pub terminal: bool,
    /// Shell extensions under "Show more options" (off = listed at the bottom of the menu).
    pub shell_submenu: bool,
}

impl Default for MenuPrefs {
    fn default() -> Self {
        Self {
            open_in_new_tab: true,
            open_in_new_window: true,
            copy_path: true,
            create_shortcut: true,
            pin: true,
            compress: true,
            tags: true,
            shelf: true,
            terminal: true,
            shell_submenu: true,
        }
    }
}

/// Details-view columns that can be hidden (name is always shown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnPrefs {
    pub modified: bool,
    pub created: bool,
    pub kind: bool,
    pub size: bool,
    /// Widths (px) of modified, created, type, size; dragged in the header.
    pub widths: [u16; 4],
    /// Files' "File tag", "Git status" and "Sync status" columns. Git shows only inside a
    /// repository, sync only in folders with cloud files.
    pub tags: bool,
    pub git: bool,
    pub sync: bool,
    /// Widths of tags, git, sync.
    pub extra_widths: [u16; 3],
    /// Files' commit columns (inside a repository): last commit message, author, date, hash.
    pub git_message: bool,
    pub git_author: bool,
    pub git_date: bool,
    pub git_hash: bool,
    pub git_widths: [u16; 4],
}

impl Default for ColumnPrefs {
    fn default() -> Self {
        Self {
            modified: true,
            created: false,
            kind: true,
            size: true,
            widths: [150, 150, 150, 100],
            tags: true,
            git: true,
            sync: true,
            extra_widths: [130, 120, 150],
            git_message: false,
            git_author: false,
            git_date: false,
            git_hash: false,
            git_widths: [240, 130, 140, 90],
        }
    }
}

/// Where the quick search's "search the web" row sends the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebSearch {
    #[default]
    Google,
    Bing,
    DuckDuckGo,
    Yandex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RendererPref {
    #[default]
    Auto,
    Gpu,
    Cpu,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemePref,
    /// UI language (`system` follows the Windows UI language).
    pub language: Lang,
    pub default_view: DefaultView,
    pub show_hidden: bool,
    pub preview_open: bool,
    pub sidebar_open: bool,
    /// The sidebar as a narrow strip of icons (drag it narrow to get there).
    pub sidebar_rail: bool,
    /// Pane widths (drag the pane edge to resize).
    pub sidebar_width: f32,
    /// Sidebar sections the user turned off ("drives", "cloud", "libraries", "network", "tags").
    pub sidebar_hidden: Vec<String>,
    pub preview_width: f32,
    pub renderer: RendererPref,
    pub restore_session: bool,
    /// Last session's tab locations ("" = Home).
    pub session_tabs: Vec<String>,
    pub session_active: usize,
    /// User pins. `None` = the default known folders.
    pub pinned: Option<Vec<PathBuf>>,
    /// Folder aliases: `@name` in the address box (Files' folder aliases).
    pub folder_aliases: std::collections::BTreeMap<String, PathBuf>,
    /// Closing the last window keeps smoothyfiles in the notification area (Files' "leave app
    /// running in the background"): the next window opens instantly.
    pub run_in_background: bool,
    /// Status bar: listing time and the GPU/CPU renderer badge (developer info).
    pub show_perf_info: bool,
    /// A notification when a long operation finishes while smoothyfiles is not in front.
    pub notify_long_ops: bool,
    /// Updates (GitHub Releases) are looked for and downloaded in the background.
    pub auto_update: bool,
    /// The one-time "star it on GitHub" strip: first start (Unix ms), starts so far, and
    /// whether it was answered (starred or dismissed).
    pub first_start_ms: i64,
    pub starts: u32,
    pub star_asked: bool,
    /// The "buy me a coffee" strip, now and then: not before this time (Unix ms; 0 = by the
    /// first-start rule), never again once turned off, and how many times it was answered (its
    /// message changes each time).
    pub donate_next_ms: i64,
    pub donate_never: bool,
    pub donate_seen: u32,
    /// The one-time move from "details" to "adaptive" (0.3.3) has been made.
    pub adaptive_offered: bool,
    pub show_extensions: bool,
    /// Custom shortcuts: keystroke → action, e.g. `"ctrl-k" = "NewFolder"` (names as in the
    /// command palette's actions; `smoothyfiles::` prefix optional). Applied at startup.
    pub keybindings: std::collections::BTreeMap<String, String>,
    /// Last window placement: [x, y, width, height, maximized (0/1)].
    pub window: Option<[f32; 5]>,

    // ---- General
    pub date_format: DatePref,
    pub startup: StartupPref,
    pub startup_pages: Vec<String>,
    /// Switch to tabs opened with Ctrl+click / middle click / "Open in new tab".
    pub focus_new_tab: bool,
    /// New tabs start split in two panes.
    pub dual_pane_new_tabs: bool,
    /// Dual pane side by side (true) or stacked (false).
    pub dual_pane_vertical: bool,
    pub menu: MenuPrefs,
    pub smooth_scroll: bool,

    // ---- Appearance
    /// A scrollbar over the Details and Grid lists.
    pub show_scrollbar: bool,
    pub animations: AnimPref,
    pub backdrop: BackdropPref,
    /// Background tint color (0xRRGGBB), `None` = the backdrop as is.
    pub tint: Option<u32>,
    pub background_image: Option<PathBuf>,
    /// 0.05 … 0.6
    pub background_opacity: f32,
    /// UI font family; `None` = Segoe UI Variable / system.
    pub font: Option<String>,
    pub show_toolbar: bool,
    pub show_statusbar: bool,

    // ---- Layout
    /// One layout/sort for every folder (off = each folder remembers its own).
    pub sync_layout: bool,
    pub default_sort: rf_core::SortKey,
    pub default_sort_desc: bool,
    pub default_group: String,
    pub folders_first: bool,
    /// Files before folders (ignored while `folders_first` is on).
    pub files_first: bool,
    pub columns: ColumnPrefs,

    // ---- Files & folders
    pub show_system_files: bool,
    /// Names starting with "." count as hidden when off.
    pub show_dot_files: bool,
    pub show_thumbnails: bool,
    pub show_checkboxes: bool,
    pub single_click: ClickPref,
    pub open_folders_in_new_tab: bool,
    pub confirm_delete: DeletePref,
    pub warn_extension_change: bool,
    pub select_on_hover: bool,
    pub double_click_up: bool,
    pub select_previous_on_up: bool,
    pub decimal_sizes: bool,
    pub calculate_folder_sizes: bool,
    /// Folders show their size on disk (files rounded up to whole clusters) instead of the sum
    /// of their file sizes.
    pub folder_size_on_disk: bool,
    /// Our own parallel engine for copies, moves to other drives and permanent deletes
    /// (off: Windows' engine for everything, like Explorer).
    pub fast_file_ops: bool,
    /// Check copies after writing them.
    pub copy_verify: CopyVerify,
    /// Copy speed limit in MB/s (0 = none).
    pub copy_speed_limit: u32,
    pub show_libraries: bool,
    /// Double-clicking a ZIP/7z/tar opens it like a folder (read-only).
    pub browse_archives: bool,
    /// Search through the Windows Search index (contents, AQS) where the folder is indexed.
    pub search_index: bool,
    /// Our own name index of the local drives: searching in subfolders takes milliseconds.
    pub name_index: bool,
    /// Folders left out of the name index (searching inside them walks the disk).
    pub index_excluded: Vec<String>,

    // ---- Developer tools
    pub ide_button: IdePref,
    /// Editor to open folders with ("code" when empty and VS Code is installed).
    pub ide_path: String,

    // ---- Advanced
    pub show_flatten: bool,

    // ---- DirectLook (Hızlı Bakış, ayrı program): ayarları kendi dosyasında (`look::config`)
    /// DirectLook kullanılsın mı (kapalıyken smoothyfiles onu başlatmaz, Boşluk bir şey yapmaz).
    pub directlook_enabled: bool,

    // ---- Quick search (quick_search.rs)
    /// A global shortcut opens the quick search: apps, files, bookmarks, the web.
    pub quick_search: bool,
    /// Its shortcut, in GPUI's syntax ("alt-space").
    pub quick_search_keys: String,
    pub web_search: WebSearch,
    /// Chrome's and Edge's bookmarks are searched too (read on this PC only).
    pub quick_search_bookmarks: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            language: Lang::System,
            default_view: DefaultView::Adaptive,
            show_hidden: false,
            preview_open: true,
            sidebar_open: true,
            sidebar_rail: false,
            sidebar_width: 232.,
            sidebar_hidden: Vec::new(),
            preview_width: 300.,
            renderer: RendererPref::Auto,
            restore_session: true,
            session_tabs: Vec::new(),
            session_active: 0,
            pinned: None,
            folder_aliases: Default::default(),
            run_in_background: false,
            show_perf_info: false,
            notify_long_ops: true,
            auto_update: true,
            first_start_ms: 0,
            starts: 0,
            star_asked: false,
            donate_next_ms: 0,
            donate_never: false,
            donate_seen: 0,
            adaptive_offered: false,
            show_extensions: true,
            keybindings: Default::default(),
            window: None,
            date_format: DatePref::Application,
            startup: StartupPref::Continue,
            startup_pages: Vec::new(),
            focus_new_tab: false,
            dual_pane_new_tabs: false,
            dual_pane_vertical: true,
            menu: MenuPrefs::default(),
            smooth_scroll: true,
            show_scrollbar: true,
            animations: AnimPref::System,
            backdrop: BackdropPref::Mica,
            tint: None,
            background_image: None,
            background_opacity: 0.2,
            font: None,
            show_toolbar: true,
            show_statusbar: true,
            sync_layout: false,
            default_sort: rf_core::SortKey::Name,
            default_sort_desc: false,
            default_group: "none".into(),
            folders_first: true,
            files_first: false,
            columns: ColumnPrefs::default(),
            show_system_files: false,
            show_dot_files: true,
            show_thumbnails: true,
            show_checkboxes: false,
            single_click: ClickPref::Never,
            open_folders_in_new_tab: false,
            confirm_delete: DeletePref::PermanentOnly,
            warn_extension_change: true,
            select_on_hover: false,
            double_click_up: true,
            select_previous_on_up: true,
            decimal_sizes: false,
            calculate_folder_sizes: false,
            folder_size_on_disk: false,
            fast_file_ops: true,
            copy_verify: CopyVerify::Off,
            copy_speed_limit: 0,
            show_libraries: false,
            browse_archives: true,
            search_index: true,
            name_index: true,
            index_excluded: Vec::new(),
            ide_button: IdePref::GitRepos,
            ide_path: String::new(),
            show_flatten: false,
            directlook_enabled: true,
            quick_search: true,
            quick_search_keys: "alt-space".into(),
            web_search: WebSearch::Google,
            quick_search_bookmarks: true,
        }
    }
}

impl Global for Settings {}

static THEME: AtomicU8 = AtomicU8::new(0);
static SOLID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static ANIM: AtomicU8 = AtomicU8::new(0);

/// Whether UI animations play (Settings → Appearance → Animations; "System" follows Windows'
/// Animation effects, read each time: it's one cheap call and may change while we run).
pub fn animations() -> bool {
    match ANIM.load(Ordering::Relaxed) {
        x if x == AnimPref::On as u8 => true,
        x if x == AnimPref::Off as u8 => false,
        _ => rf_win::sysparams::client_area_animation(),
    }
}

/// "Solid" background material: the palette paints an opaque window background.
pub fn solid_backdrop() -> bool {
    SOLID.load(Ordering::Relaxed)
}

/// Read by `Palette::for_window` without needing an `App`.
pub fn theme_override() -> ThemePref {
    match THEME.load(Ordering::Relaxed) {
        1 => ThemePref::Light,
        2 => ThemePref::Dark,
        _ => ThemePref::System,
    }
}

pub fn path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("settings.toml"))
}

impl Settings {
    pub fn load() -> Self {
        let s: Settings = path()
            .and_then(|p| {
                let text = match rf_core::read_config(&p) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                    Err(e) => {
                        // unreadable (not UTF-8, locked): keep it aside rather than overwrite it
                        log::warn!("settings: {e}");
                        rf_core::backup_corrupt(&p);
                        return None;
                    }
                };
                match Self::parse_lenient(text.trim_start_matches('\u{feff}')) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        // a typo while hand-editing must not cost the user every setting:
                        // keep the file aside, start with defaults
                        log::warn!("settings: {e}");
                        rf_core::backup_corrupt(&p);
                        None
                    }
                }
            })
            .unwrap_or_default();
        let mut s = s;
        s.migrate();
        s.apply_side_effects();
        s
    }

    /// Brings settings written by older versions up to date.
    fn migrate(&mut self) {
        // older files: "restore_session = false" means "start with a new tab"
        if !self.restore_session && self.startup == StartupPref::Continue {
            self.startup = StartupPref::NewTab;
            self.restore_session = true;
        }
        // "details" was the default before 0.3.3, so it says nothing about a choice: move it
        // once to "adaptive" (plain folders still open as details). Grid and columns stay.
        if !self.adaptive_offered {
            if self.default_view == DefaultView::Details {
                self.default_view = DefaultView::Adaptive;
            }
            self.adaptive_offered = true;
        }
    }

    /// Field by field: a value this version doesn't understand (a newer build's theme, a
    /// mistyped enum) falls back to its default instead of resetting every setting. Only a
    /// TOML syntax error fails.
    fn parse_lenient(text: &str) -> Result<Settings, toml::de::Error> {
        let table: toml::Table = toml::from_str(text)?;
        let mut base = toml::Table::try_from(Settings::default()).unwrap_or_default();
        for (k, v) in table {
            let mut trial = base.clone();
            trial.insert(k.clone(), v);
            if Settings::deserialize(toml::Value::Table(trial.clone())).is_ok() {
                base = trial;
            } else {
                log::warn!("settings: ignoring invalid value for '{k}'");
            }
        }
        Ok(Settings::deserialize(toml::Value::Table(base)).unwrap_or_default())
    }

    /// For "Import settings".
    pub fn parse_lenient_pub(text: &str) -> Result<Settings, toml::de::Error> {
        Self::parse_lenient(text)
    }

    fn apply_side_effects(&self) {
        sync_name_index(self.name_index, &self.index_excluded);
        crate::i18n::set(self.language);
        crate::aliases::set(self.folder_aliases.clone());
        crate::background::sync_tray(self.run_in_background);
        crate::quick_search::sync(self);
        rf_core::format::set_date_style(match self.date_format {
            DatePref::Application => rf_core::format::DateStyle::Relative,
            DatePref::System => rf_core::format::DateStyle::Local,
            DatePref::Universal => rf_core::format::DateStyle::Universal,
        });
        rf_core::format::set_decimal_sizes(self.decimal_sizes);
        SOLID.store(self.backdrop == BackdropPref::Solid, Ordering::Relaxed);
        ANIM.store(self.animations as u8, Ordering::Relaxed);
        gpui_windows::SMOOTH_SCROLL.store(self.smooth_scroll, Ordering::Relaxed);
        THEME.store(
            match self.theme {
                ThemePref::System => 0,
                ThemePref::Light => 1,
                ThemePref::Dark => 2,
            },
            Ordering::Relaxed,
        );
        // An explicit SMOOTHYFILES_RENDERER env var wins (debugging).
        if std::env::var_os("SMOOTHYFILES_RENDERER").is_none() {
            use gpui_windows::hybrid::{RendererMode, set_mode};
            set_mode(match self.renderer {
                RendererPref::Auto => RendererMode::Auto,
                RendererPref::Gpu => RendererMode::Gpu,
                RendererPref::Cpu => RendererMode::Cpu,
            });
        }
    }

    pub fn save(&self) {
        // One writer at a time; a unique temp name per write (saves come from several threads).
        static LOCK: parking_lot::Mutex<u64> = parking_lot::Mutex::new(0);
        let mut n = LOCK.lock();
        *n += 1;
        let Some(p) = path() else { return };
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                // write-then-rename so a crash never leaves a half-written file
                let tmp = p.with_extension(format!("toml.{}.{}.tmp", std::process::id(), *n));
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
            Err(e) => log::warn!("settings save: {e}"),
        }
    }
}

/// Keeps gpui-component widgets (inputs, menus) in the same light/dark mode as our palette.
pub fn apply_component_theme(cx: &mut App) {
    use gpui_kit::component::theme::{Theme, ThemeMode};
    match cx.global::<Settings>().theme {
        ThemePref::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemePref::Dark => Theme::change(ThemeMode::Dark, None, cx),
        ThemePref::System => Theme::sync_system_appearance(None, cx),
    }
    // The component root paints the theme background under every window; keep it clear so
    // the Mica backdrop shows through (our own surfaces bring their own fills).
    Theme::update(cx, |t| t.background = gpui_kit::transparent_black());
    // the chosen font, else Windows 11's own typeface (optical sizes) when present
    if let Some(f) = cx.global::<Settings>().font.clone() {
        Theme::update(cx, |t| t.font_family = f.into());
    } else if std::path::Path::new(r"C:\Windows\Fonts\SegUIVar.ttf").exists() {
        Theme::update(cx, |t| t.font_family = "Segoe UI Variable Text".into());
    }
}

/// Mutates the global settings, applies side effects, saves in the background and refreshes windows.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    cx.update_global::<Settings, _>(|s, _| f(s));
    let s = cx.global::<Settings>().clone();
    s.apply_side_effects();
    apply_component_theme(cx);
    std::thread::spawn(move || s.save());
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    #[test]
    fn details_moves_to_adaptive_once() {
        // a file from before the move: details becomes adaptive
        let mut s = super::Settings::parse_lenient("default_view = \"details\"\n").unwrap();
        s.migrate();
        assert_eq!(s.default_view, super::DefaultView::Adaptive);
        assert!(s.adaptive_offered);
        // chosen afterwards: stays
        let mut s = super::Settings::parse_lenient("default_view = \"details\"\nadaptive_offered = true\n").unwrap();
        s.migrate();
        assert_eq!(s.default_view, super::DefaultView::Details);
        // grid was a real choice
        let mut s = super::Settings::parse_lenient("default_view = \"grid\"\n").unwrap();
        s.migrate();
        assert_eq!(s.default_view, super::DefaultView::Grid);
    }

    use super::{Settings, ThemePref};

    #[test]
    fn roundtrip_and_defaults() {
        let s = Settings { show_hidden: true, session_tabs: vec!["C:\\".into()], ..Default::default() };
        let t = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&t).unwrap();
        assert!(back.show_hidden);
        assert_eq!(back.session_tabs, vec!["C:\\".to_string()]);
        // unknown/missing keys fall back to defaults
        let partial: Settings = toml::from_str("theme = \"dark\"").unwrap();
        assert_eq!(partial.theme, ThemePref::Dark);
        assert!(partial.restore_session);
    }
}

/// Where data that can be rebuilt lives (remembered folder sizes): `%LOCALAPPDATA%\smoothyfiles`,
/// or `SMOOTHYFILES_CONFIG_DIR` when set, so test copies keep their data apart.
pub fn local_data_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("SMOOTHYFILES_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(std::path::PathBuf::from(d));
    }
    dirs::data_local_dir().map(|d| d.join("smoothyfiles"))
}

/// `%APPDATA%\smoothyfiles`, or `SMOOTHYFILES_CONFIG_DIR` when set (portable use, tests).
pub fn app_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("SMOOTHYFILES_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(std::path::PathBuf::from(d));
    }
    dirs::config_dir().map(|d| d.join("smoothyfiles"))
}

#[cfg(test)]
mod lenient_tests {
    use super::Settings;

    #[test]
    fn unknown_values_fall_back_per_field() {
        let s = Settings::parse_lenient("theme = \"mor\"
show_hidden = true
sidebar_width = \"geniş\"
yeni_alan = 3").unwrap();
        assert!(s.show_hidden, "valid fields survive");
        assert_eq!(s.sidebar_width, Settings::default().sidebar_width);
        assert!(Settings::parse_lenient("[[[bozuk").is_err(), "syntax errors still fail");
    }
}

/// Starts or stops the name index when the setting changes. The first start waits a few seconds
/// so it never competes with opening the window.
fn sync_name_index(on: bool, excluded: &[String]) {
    use std::sync::atomic::{AtomicU8, Ordering};
    // DirectLook reads these settings too: the index is smoothyfiles' (a second indexer would walk
    // every drive again and write the same index files)
    if crate::background::DIRECTLOOK_PROCESS.load(Ordering::Relaxed) {
        return;
    }
    // 0 = not decided yet, 1 = off, 2 = on
    static STATE: AtomicU8 = AtomicU8::new(0);
    static EXCLUDED: parking_lot::Mutex<Option<Vec<String>>> = parking_lot::Mutex::new(None);
    let list_changed = {
        let mut e = EXCLUDED.lock();
        let changed = e.as_deref() != Some(excluded);
        if changed {
            *e = Some(excluded.to_vec());
            rf_win::nameindex::set_excluded(&excluded.iter().map(std::path::PathBuf::from).collect::<Vec<_>>());
        }
        changed
    };
    let want = if on { 2 } else { 1 };
    let was = STATE.swap(want, Ordering::Relaxed);
    // a changed exclusion list rebuilds a running index (the fresh walk leaves the folders out)
    if was == want && !(on && list_changed && was != 0) {
        return;
    }
    if !on {
        std::thread::spawn(rf_win::indexer::stop);
        return;
    }
    let delay = if was == 0 { 5 } else { 0 };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(delay));
        // extra instances (tests, the speed test's) leave it to the main one
        // (SMOOTHYFILES_INDEX=1 indexes there too, for testing)
        let extra = std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_some() && std::env::var_os("SMOOTHYFILES_INDEX").is_none();
        if STATE.load(Ordering::Relaxed) != 2 || std::env::var_os("SMOOTHYFILES_NO_INDEX").is_some() || extra {
            return;
        }
        // a copy with its own settings folder (tests, portable) keeps its index there too
        let dir = match std::env::var_os("SMOOTHYFILES_CONFIG_DIR") {
            Some(cfg) => std::path::PathBuf::from(cfg).join("index"),
            None => std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, std::path::PathBuf::from).join("smoothyfiles").join("index"),
        };
        // SMOOTHYFILES_INDEX_ROOTS="E:\;F:\" picks the roots (testing)
        let roots = match std::env::var("SMOOTHYFILES_INDEX_ROOTS") {
            Ok(v) => v.split(';').filter(|s| !s.is_empty()).map(std::path::PathBuf::from).collect(),
            Err(_) => rf_win::indexer::local_roots(),
        };
        rf_win::indexer::start(roots, dir);
    });
}
