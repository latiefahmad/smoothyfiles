//! Quick search (Alt+Space by default): a small window over whatever app is in front that finds
//! installed apps, files and folders (smoothyfiles' name index), Chrome / Edge bookmarks, and
//! offers a web search. Enter opens, Ctrl+Enter shows the item in its folder, Shift+Enter
//! previews it with DirectLook, Esc closes. What is opened often comes first.
//!
//! The hotkey is registered by the running smoothyfiles (`rf_win::hotkey`); with "keep running in
//! the background" on it works while no window is open.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use crate::image_cache::ImageCache;
use crate::settings::{Settings, WebSearch};
use crate::t;
use crate::theme::{self, Palette};

/// The shortcut editor records into the quick-search hotkey under this name (keymap_editor).
pub const CAPTURE_ACTION: &str = "@quick-search";
/// `smoothyfiles.exe --quick-search` opens it too (in the running smoothyfiles).
pub const LAUNCH_ARG: &str = "--quick-search";

const WIDTH: f32 = 680.;
const ROW_H: f32 = 46.;
/// Results per kind.
const MAX_APPS: usize = 4;
const MAX_FILES: usize = 7;
const MAX_BOOKMARKS: usize = 3;
/// An app list older than this is refreshed (in the background) when the window opens.
const APPS_STALE: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------------------------
// The hotkey

/// The shortcut held now (None: none) and whether registering it failed.
static HELD: Mutex<Option<(Option<String>, Option<rf_win::hotkey::HotkeyError>)>> = Mutex::new(None);

static NO_HOTKEY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A side copy of smoothyfiles (the taskbar preview) must not take the user's hotkey.
pub fn disable_hotkey() {
    NO_HOTKEY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// This process registers the hotkey: the one smoothyfiles (not DirectLook, not a test copy).
fn hotkey_owner() -> bool {
    !crate::background::DIRECTLOOK_PROCESS.load(std::sync::atomic::Ordering::Relaxed)
        && !NO_HOTKEY.load(std::sync::atomic::Ordering::Relaxed)
        && std::env::var_os("SMOOTHYFILES_MULTI_INSTANCE").is_none()
}

/// Apps, files and pages being started on other threads: the app does not quit under them
/// (started only for the quick search, it quits when its window closes).
static LAUNCHING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn launching() -> usize {
    LAUNCHING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Runs `f` on a thread of its own, counted in `launching`.
fn launch(f: impl FnOnce() + Send + 'static) {
    LAUNCHING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::thread::spawn(move || {
        f();
        LAUNCHING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Settings applied: holds the configured hotkey (or none). Registering waits for the hotkey
/// thread, a few microseconds.
pub fn sync(s: &Settings) {
    if !hotkey_owner() {
        return;
    }
    let want = s.quick_search.then(|| s.quick_search_keys.clone()).filter(|k| !k.trim().is_empty());
    let mut held = HELD.lock().unwrap();
    // (a shortcut another app held is tried again: that app may have let it go)
    if held.as_ref().is_some_and(|(k, e)| *k == want && e.is_none()) {
        return;
    }
    let r = rf_win::hotkey::set(want.as_deref());
    if let Err(e) = &r {
        log::warn!("quick search hotkey {want:?}: {e:?}");
    }
    *held = Some((want, r.err()));
}

/// Lets the next `sync` register again (after the shortcut editor let go of the keys).
pub fn resync(cx: &App) {
    *HELD.lock().unwrap() = None;
    sync(cx.global::<Settings>());
}

/// The shortcut editor is recording a new hotkey: the old one must not swallow the keys.
pub fn release_for_capture() {
    if !hotkey_owner() {
        return;
    }
    let _ = rf_win::hotkey::set(None);
    *HELD.lock().unwrap() = Some((None, None));
}

/// Why the configured hotkey does not work, if it doesn't.
pub fn problem() -> Option<rf_win::hotkey::HotkeyError> {
    HELD.lock().unwrap().as_ref().and_then(|(_, e)| e.clone())
}

/// Routes hotkey presses to the app and warms the app list (call once at startup).
pub fn init(cx: &mut App) {
    if !hotkey_owner() {
        return;
    }
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
    rf_win::hotkey::set_handler(move || {
        let _ = tx.unbounded_send(());
    });
    cx.spawn(async move |cx| {
        use futures::StreamExt;
        while rx.next().await.is_some() {
            cx.update(toggle);
        }
    })
    .detach();
    // the first press should not wait for "All apps" (about a second cold)
    if cx.global::<Settings>().quick_search {
        std::thread::Builder::new()
            .name("quick-search-warm".into())
            .spawn(|| {
                std::thread::sleep(Duration::from_secs(8));
                let _ = apps();
            })
            .ok();
    }
}

// ---------------------------------------------------------------------------------------------
// Sources

static APPS: Mutex<Option<(Instant, Arc<Vec<rf_win::apps::App>>)>> = Mutex::new(None);
/// One enumeration at a time (each keystroke's search would start its own).
static LISTING: Mutex<()> = Mutex::new(());

/// "All apps": cached; a stale list is served while a fresh one is read.
fn apps() -> Arc<Vec<rf_win::apps::App>> {
    let cached = APPS.lock().unwrap().clone();
    match cached {
        Some((at, list)) => {
            if at.elapsed() > APPS_STALE {
                *APPS.lock().unwrap() = Some((Instant::now(), list.clone()));
                std::thread::spawn(|| {
                    if let Ok(l) = rf_win::apps::list() {
                        *APPS.lock().unwrap() = Some((Instant::now(), Arc::new(l)));
                    }
                });
            }
            list
        }
        None => {
            let _one = LISTING.lock().unwrap();
            if let Some((_, list)) = APPS.lock().unwrap().clone() {
                return list;
            }
            match rf_win::apps::list() {
                Ok(l) if !l.is_empty() => {
                    let list = Arc::new(l);
                    *APPS.lock().unwrap() = Some((Instant::now(), list.clone()));
                    list
                }
                // not cached: the shell may not be ready yet, the next search asks again
                _ => Arc::new(Vec::new()),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bookmark {
    pub name: String,
    pub url: String,
}

/// Chrome's and Edge's bookmark files (every profile), with their change times: re-read only
/// when one changes.
static BOOKMARKS: Mutex<Option<(Vec<(PathBuf, Option<SystemTime>)>, Arc<Vec<Bookmark>>)>> = Mutex::new(None);

/// The browsers' profile list changes rarely: read at most every 30 s (not per keystroke).
fn bookmark_files() -> Vec<PathBuf> {
    static FILES: Mutex<Option<(Instant, Vec<PathBuf>)>> = Mutex::new(None);
    if let Some((at, f)) = FILES.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(30) {
            return f.clone();
        }
    }
    let f = find_bookmark_files();
    *FILES.lock().unwrap() = Some((Instant::now(), f.clone()));
    f
}

fn find_bookmark_files() -> Vec<PathBuf> {
    let Some(local) = dirs::data_local_dir() else { return Vec::new() };
    let mut out = Vec::new();
    for browser in ["Google\\Chrome", "Microsoft\\Edge"] {
        let root = local.join(browser).join("User Data");
        // the profiles the browser knows (Local State → profile.info_cache), else Default
        let profiles: Vec<String> = std::fs::read(root.join("Local State"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.pointer("/profile/info_cache").and_then(|c| c.as_object()).map(|o| o.keys().cloned().collect()))
            .unwrap_or_else(|| vec!["Default".to_string()]);
        for p in profiles {
            let f = root.join(p).join("Bookmarks");
            if f.is_file() {
                out.push(f);
            }
        }
    }
    out
}

fn collect_urls(node: &serde_json::Value, out: &mut Vec<Bookmark>) {
    match node.get("type").and_then(|t| t.as_str()) {
        Some("url") => {
            let url = node.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if url.starts_with("https://") || url.starts_with("http://") {
                let name = node.get("name").and_then(|n| n.as_str()).unwrap_or("").trim();
                out.push(Bookmark { name: if name.is_empty() { url.to_string() } else { name.to_string() }, url: url.to_string() });
            }
        }
        _ => {
            if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
                for c in children {
                    collect_urls(c, out);
                }
            }
        }
    }
}

/// The bookmarks of one browser profile (`Bookmarks`, the browser's JSON).
pub fn parse_bookmarks(json: &[u8]) -> Vec<Bookmark> {
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) {
        if let Some(roots) = v.get("roots").and_then(|r| r.as_object()) {
            for root in roots.values().filter(|r| r.is_object()) {
                collect_urls(root, &mut out);
            }
        }
    }
    out
}

fn bookmarks() -> Arc<Vec<Bookmark>> {
    let files: Vec<(PathBuf, Option<SystemTime>)> =
        bookmark_files().into_iter().map(|f| { let m = std::fs::metadata(&f).ok().and_then(|m| m.modified().ok()); (f, m) }).collect();
    if let Some((stamp, list)) = BOOKMARKS.lock().unwrap().as_ref() {
        if *stamp == files {
            return list.clone();
        }
    }
    let mut all = Vec::new();
    for (f, _) in &files {
        if let Ok(b) = std::fs::read(f) {
            all.extend(parse_bookmarks(&b));
        }
    }
    // the same page saved in two browsers or profiles shows once
    let mut seen = std::collections::HashSet::new();
    all.retain(|b| seen.insert(b.url.clone()));
    let list = Arc::new(all);
    *BOOKMARKS.lock().unwrap() = Some((files, list.clone()));
    list
}

// ---------------------------------------------------------------------------------------------
// What was opened, and how often (`quicksearch.json` next to settings.toml)

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Use {
    count: u32,
    /// Seconds since 1970.
    last: u64,
    title: String,
    detail: String,
    target: Target,
}

#[derive(Default)]
struct Usage {
    map: HashMap<String, Use>,
    loaded: bool,
}

static USAGE: std::sync::LazyLock<Mutex<Usage>> = std::sync::LazyLock::new(|| Mutex::new(Usage::default()));

fn usage_path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("quicksearch.json"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn with_usage<R>(f: impl FnOnce(&mut HashMap<String, Use>) -> R) -> R {
    let mut u = USAGE.lock().unwrap();
    if !u.loaded {
        u.loaded = true;
        if let Some(text) = usage_path().and_then(|p| std::fs::read(p).ok()) {
            // entry by entry: one this version can't read does not cost the others
            let raw: HashMap<String, serde_json::Value> = serde_json::from_slice(&text).unwrap_or_default();
            u.map = raw.into_iter().filter_map(|(k, v)| serde_json::from_value(v).ok().map(|u| (k, u))).collect();
        }
    }
    f(&mut u.map)
}

/// How much use lifts an item: grows with the count, fades over weeks without use.
fn boost(u: &Use) -> i32 {
    let days = now_secs().saturating_sub(u.last) as f64 / 86_400.0;
    let recency = 1.0 / (1.0 + days / 14.0);
    ((u.count as f64).ln_1p() * 60.0 * recency).min(240.0) as i32
}

fn remember(item: &Item) {
    // a typed web search is not something to offer again (and stays off the disk)
    if matches!(item.target, Target::Web { .. }) {
        return;
    }
    let key = item.target.key();
    let snapshot = with_usage(|m| {
        let e = m.entry(key).or_insert_with(|| Use { count: 0, last: 0, title: item.title.clone(), detail: item.detail.clone(), target: item.target.clone() });
        e.count += 1;
        e.last = now_secs();
        e.title = item.title.clone();
        e.detail = item.detail.clone();
        // a long tail of one-off items is not worth keeping
        if m.len() > 600 {
            let mut v: Vec<(String, i32, u64)> = m.iter().map(|(k, u)| (k.clone(), boost(u), u.last)).collect();
            v.sort_by_key(|(_, b, last)| (*b, *last));
            for (k, _, _) in v.into_iter().take(m.len() - 500) {
                m.remove(&k);
            }
        }
        serde_json::to_vec(m).ok()
    });
    if let (Some(bytes), Some(path)) = (snapshot, usage_path()) {
        std::thread::spawn(move || {
            static WRITE: Mutex<()> = Mutex::new(());
            let _g = WRITE.lock().unwrap();
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Matching and ranking (pure)

/// Lower case without Turkish / accented letters' marks: "Şarkı" and "sarki" match.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| *c != '\u{307}')
        .map(|c| match c {
            'ş' => 's',
            'ğ' => 'g',
            'ü' | 'û' => 'u',
            'ö' => 'o',
            'ç' => 'c',
            'ı' | 'î' => 'i',
            'â' | 'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            c => c,
        })
        .collect()
}

/// How well `name` matches the folded query `q`: whole name, start, start of a word, anywhere.
/// `None`: no match.
pub fn score(name: &str, q: &str) -> Option<i32> {
    score_with(name, q, false)
}

/// `score`, and for app names also the initials of their words ("vsc" → Visual Studio Code;
/// for page titles and file names that matched too much by chance).
fn score_with(name: &str, q: &str, initials: bool) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let n = fold(name);
    let short = (40 - n.chars().count() as i32).max(0);
    if n == q {
        return Some(1000);
    }
    if n.starts_with(q) {
        return Some(800 + short);
    }
    let is_start = |i: usize| i == 0 || !n[..i].chars().last().is_some_and(char::is_alphanumeric);
    if let Some(i) = n.match_indices(q).map(|(i, _)| i).find(|&i| is_start(i)) {
        let _ = i;
        return Some(600 + short);
    }
    if let Some(i) = n.find(q) {
        return Some(400 - (i as i32).min(100) + short / 2);
    }
    if !initials {
        return None;
    }
    // initials: each query letter starts a word, in order
    let starts: Vec<char> = n.char_indices().filter(|(i, _)| is_start(*i)).map(|(_, c)| c).collect();
    let mut it = starts.iter();
    if q.chars().all(|qc| it.any(|&c| c == qc)) && q.chars().count() >= 2 {
        return Some(300 + short / 2);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    App { id: String, name: String },
    Path { path: PathBuf, dir: bool },
    Url { url: String },
    Web { query: String },
}

impl Target {
    fn key(&self) -> String {
        match self {
            Target::App { id, .. } => format!("app:{id}"),
            Target::Path { path, .. } => format!("path:{}", path.to_string_lossy().to_lowercase()),
            Target::Url { url } => format!("url:{url}"),
            Target::Web { query } => format!("web:{query}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Recent,
    Apps,
    Files,
    Bookmarks,
    Web,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub target: Target,
    pub title: String,
    pub detail: String,
    pub section: Section,
    pub score: i32,
}

/// Everything found for one query, in the order shown.
pub struct Found {
    pub items: Vec<Item>,
    /// The file index is still being built: files may be missing.
    pub index_building: bool,
    /// No file index (turned off in Settings): no files at all.
    pub index_off: bool,
}

fn web_url(engine: WebSearch, q: &str) -> String {
    let enc: String = q
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    match engine {
        WebSearch::Google => format!("https://www.google.com/search?q={enc}"),
        WebSearch::Bing => format!("https://www.bing.com/search?q={enc}"),
        WebSearch::DuckDuckGo => format!("https://duckduckgo.com/?q={enc}"),
        WebSearch::Yandex => format!("https://yandex.com/search/?text={enc}"),
    }
}

fn engine_name(engine: WebSearch) -> &'static str {
    match engine {
        WebSearch::Google => "Google",
        WebSearch::Bing => "Bing",
        WebSearch::DuckDuckGo => "DuckDuckGo",
        WebSearch::Yandex => "Yandex",
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Searches every source for `query` (blocks: run off the UI thread).
fn search(query: &str, engine: WebSearch, with_bookmarks: bool, index_on: bool) -> Found {
    let q = fold(query.trim());
    let used: HashMap<String, Use> = with_usage(|m| m.clone());
    let lift = |t: &Target| used.get(&t.key()).map_or(0, boost);
    let mut items = Vec::new();
    if q.is_empty() {
        // nothing typed: what was opened most lately
        let mut v: Vec<&Use> = used.values().filter(|u| !matches!(u.target, Target::Web { .. })).collect();
        v.sort_by_key(|u| std::cmp::Reverse(boost(u)));
        // a file deleted since is left out
        for u in v.into_iter().filter(|u| !matches!(&u.target, Target::Path { path, .. } if !path.exists())).take(8) {
            items.push(Item { target: u.target.clone(), title: u.title.clone(), detail: u.detail.clone(), section: Section::Recent, score: 0 });
        }
        return Found { items, index_building: false, index_off: false };
    }
    // apps
    let mut a: Vec<Item> = apps()
        .iter()
        .filter_map(|app| {
            let t = Target::App { id: app.id.clone(), name: app.name.clone() };
            score_with(&app.name, &q, true).map(|s| Item { score: s + lift(&t), title: app.name.clone(), detail: String::new(), section: Section::Apps, target: t })
        })
        .collect();
    a.sort_by_key(|i| std::cmp::Reverse(i.score));
    items.extend(a.into_iter().take(MAX_APPS));
    // files and folders: the name index, every indexed drive
    let mut index_building = false;
    let status = rf_win::indexer::status();
    // on in Settings but not started yet (it starts a few seconds after smoothyfiles): coming
    let index_off = !index_on;
    if index_on && status.is_empty() {
        index_building = true;
    }
    // a wildcard query ("*.psd") is the index's glob: its hits all match equally
    let glob = query.contains(['*', '?']);
    let mut hits: Vec<(i32, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |p: PathBuf, hits: &mut Vec<(i32, PathBuf)>| {
        if !seen.insert(p.clone()) {
            return;
        }
        let s = if glob { Some(500) } else { score(&file_name(&p), &q) };
        if let Some(s) = s {
            // a shallower path is likelier what is meant
            let depth = p.components().count() as i32;
            hits.push((s - depth * 4, p));
        }
    };
    for (root, state, _, _) in status {
        match state {
            rf_win::indexer::State::Ready => {}
            rf_win::indexer::State::Building => {
                index_building = true;
                continue;
            }
            _ => continue,
        }
        // the best names first, whatever their place in the index (a common fragment matches
        // tens of thousands of files)
        let rank = |name: &str| if glob { Some(500) } else { score(name, &q) };
        if let Some(found) = rf_win::indexer::search_ranked(query.trim(), &root, false, 60, &rank) {
            for (_, p) in found {
                add(p, &mut hits);
            }
        }
    }
    // what was opened before is always a candidate (the cap above could leave it out); only a
    // name that matches is looked for on the disk
    for u in used.values() {
        if let Target::Path { path, .. } = &u.target {
            if (glob || score(&file_name(path), &q).is_some()) && path.exists() {
                add(path.clone(), &mut hits);
            }
        }
    }
    let mut f: Vec<Item> = hits
        .into_iter()
        .map(|(s, p)| {
            let target = Target::Path { path: p.clone(), dir: false };
            Item { score: s + lift(&target), title: file_name(&p), detail: p.parent().map(|d| d.display().to_string()).unwrap_or_default(), section: Section::Files, target }
        })
        .collect();
    f.sort_by_key(|i| std::cmp::Reverse(i.score));
    f.truncate(MAX_FILES);
    for i in &mut f {
        if let Target::Path { path, dir } = &mut i.target {
            *dir = std::fs::metadata(&*path).is_ok_and(|m| m.is_dir());
        }
    }
    items.extend(f);
    // bookmarks
    if with_bookmarks {
        let mut b: Vec<Item> = bookmarks()
            .iter()
            .filter_map(|bm| {
                let host = bm.url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("").trim_start_matches("www.");
                let s = score(&bm.name, &q).or_else(|| score(host, &q).map(|s| s - 100))?;
                let t = Target::Url { url: bm.url.clone() };
                Some(Item { score: s + lift(&t), title: bm.name.clone(), detail: host.to_string(), section: Section::Bookmarks, target: t })
            })
            .collect();
        b.sort_by_key(|i| std::cmp::Reverse(i.score));
        items.extend(b.into_iter().take(MAX_BOOKMARKS));
    }
    // the web, always last
    items.push(Item {
        target: Target::Web { query: query.trim().to_string() },
        title: query.trim().to_string(),
        detail: engine_name(engine).to_string(),
        section: Section::Web,
        score: 0,
    });
    Found { items, index_building, index_off }
}

// ---------------------------------------------------------------------------------------------
// The window

struct Host {
    window: Option<WindowHandle<QuickSearch>>,
    images: Option<Entity<ImageCache>>,
}
impl Global for Host {}

fn host(cx: &mut App) -> &mut Host {
    if !cx.has_global::<Host>() {
        cx.set_global(Host { window: None, images: None });
    }
    cx.global_mut::<Host>()
}

pub fn close(cx: &mut App) {
    if let Some(w) = host(cx).window.take() {
        let _ = w.update(cx, |_, window, _| window.remove_window());
    }
}

/// The hotkey: opens the window, or closes it when it is open.
pub fn toggle(cx: &mut App) {
    if let Some(w) = host(cx).window {
        // (closed some other way, Alt+F4: the press opens a new one)
        if w.update(cx, |_, _, _| ()).is_ok() {
            close(cx);
            return;
        }
        host(cx).window = None;
    }
    let images = match host(cx).images.clone() {
        Some(i) => i,
        None => {
            let i = cx.new(ImageCache::new);
            host(cx).images = Some(i.clone());
            i
        }
    };
    // centered on the screen under the pointer, in its upper part (where the eyes are)
    let work = rf_win::look::win::work_area_at_cursor();
    let origin = point(px(work[0] + (work[2] - WIDTH) / 2.), px(work[1] + work[3] * 0.22));
    let max_h = (work[3] * 0.78 - 24.).max(200.);
    let display_id = cx
        .displays()
        .into_iter()
        .find(|d| {
            let b = d.visible_bounds();
            (f32::from(b.origin.x) - work[0]).abs() < 2.0 && (f32::from(b.origin.y) - work[1]).abs() < 2.0
        })
        .map(|d| d.id());
    // automated UI tests: shown without taking the foreground from the user
    let test = std::env::var_os("SMOOTHYFILES_QUICK_SEARCH_NOACTIVATE").is_some();
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, size(px(WIDTH), px(64.))))),
        display_id,
        titlebar: None,
        focus: !test,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    match cx.open_window(opts, move |window, cx| {
        let hwnd = crate::look::window::hwnd_of(window);
        rf_win::window::set_dark_mode(hwnd, Palette::for_window(window).dark);
        // summoned from another app: take the keyboard (the hotkey press allows it)
        if !test {
            rf_win::window::bring_to_front(hwnd);
        }
        cx.new(|cx| QuickSearch::new(images, max_h, window, cx))
    }) {
        Ok(w) => host(cx).window = Some(w),
        Err(e) => log::warn!("quick search window: {e:#}"),
    }
}

pub struct QuickSearch {
    input: Entity<InputState>,
    images: Entity<ImageCache>,
    items: Vec<Item>,
    index_building: bool,
    index_off: bool,
    selected: usize,
    generation: u64,
    /// The search `items` came from: Enter pressed before the newest results arrive waits
    /// for them (it would open the previous query's result).
    items_generation: u64,
    pending: Option<How>,
    /// The window's tallest size (from its top to the screen's bottom): the list scrolls past it.
    max_h: f32,
    scroll: ScrollHandle,
    /// The keyboard moved the selection: bring it into view.
    reveal: bool,
    _search: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl QuickSearch {
    fn new(images: Entity<ImageCache>, max_h: f32, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("Uygulama, dosya ya da web'de ara")));
        let mut subs = vec![cx.subscribe_in(&input, window, |this: &mut Self, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                this.run_search(cx);
            }
        })];
        subs.push(cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() {
                cx.defer(close);
            }
        }));
        subs.push(cx.observe(&images, |_, _, cx| cx.notify()));
        input.update(cx, |s, cx| s.focus(window, cx));
        let mut this = Self {
            input,
            images,
            items: Vec::new(),
            index_building: false,
            index_off: false,
            selected: 0,
            generation: 0,
            items_generation: 0,
            pending: None,
            max_h,
            scroll: ScrollHandle::new(),
            reveal: false,
            _search: None,
            _subs: subs,
        };
        this.run_search(cx);
        this
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.input.read(cx).value().to_string();
        let s = cx.global::<Settings>();
        let (engine, with_bookmarks, index_on) = (s.web_search, s.quick_search_bookmarks, s.name_index);
        let work = cx.background_spawn(async move { search(&query, engine, with_bookmarks, index_on) });
        self._search = Some(cx.spawn(async move |this, cx| {
            let found = work.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.items = found.items;
                this.index_building = found.index_building;
                this.index_off = found.index_off;
                this.items_generation = generation;
                this.selected = 0;
                this.reveal = true;
                cx.notify();
                if let Some(how) = this.pending.take() {
                    this.activate(how, cx);
                }
            });
        }));
    }

    /// Enter (`how` = Open), Ctrl+Enter (ShowInFolder), Shift+Enter (Preview).
    fn activate(&mut self, how: How, cx: &mut Context<Self>) {
        if self.items_generation != self.generation {
            self.pending = Some(how);
            return;
        }
        let Some(item) = self.items.get(self.selected).cloned() else { return };
        let engine = cx.global::<Settings>().web_search;
        match (&item.target, how) {
            (Target::App { id, name }, _) => {
                let app = rf_win::apps::App { id: id.clone(), name: name.clone() };
                launch(move || {
                    if let Err(e) = rf_win::apps::launch(&app) {
                        log::warn!("quick search: start {}: {e:#}", app.name);
                    }
                });
            }
            (Target::Path { path, .. }, How::ShowInFolder) => {
                let msg = format!("{}{}", crate::workspace::SELECT_HERE_PREFIX, path.display());
                cx.defer(move |cx| crate::workspace::route_ipc(msg, cx));
            }
            (Target::Path { path, .. }, How::Preview) => {
                rf_win::look::win::allow_any_foreground();
                crate::look::ipc::send(crate::look::ipc::Msg::Show(path.clone(), crate::look::ShowOpts::default()), true);
            }
            (Target::Path { path, dir: true }, How::Open) => {
                // a folder opens in smoothyfiles
                let msg = path.display().to_string();
                cx.defer(move |cx| crate::workspace::route_ipc(msg, cx));
            }
            (Target::Path { path, dir: false }, How::Open) => {
                let p = path.clone();
                launch(move || {
                    if let Err(e) = rf_win::shell::open(&p) {
                        log::warn!("quick search: open {}: {e:#}", p.display());
                    }
                });
            }
            (Target::Url { url }, _) => open_url(url.clone()),
            (Target::Web { query }, _) => open_url(web_url(engine, query)),
        }
        if !matches!(how, How::Preview) {
            remember(&item);
            close_later(cx);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum How {
    Open,
    ShowInFolder,
    Preview,
}

fn open_url(url: String) {
    launch(move || {
        if let Err(e) = rf_win::shell::open(Path::new(&url)) {
            log::warn!("quick search: open url: {e:#}");
        }
    });
}

fn close_later<T>(cx: &mut Context<T>) {
    cx.defer(close);
}

fn section_title(s: Section) -> &'static str {
    match s {
        Section::Recent => t!("Son kullanılanlar"),
        Section::Apps => t!("Uygulamalar"),
        Section::Files => t!("Dosyalar ve klasörler"),
        Section::Bookmarks => t!("Yer imleri"),
        Section::Web => t!("Web"),
    }
}

impl QuickSearch {
    fn icon(&self, item: &Item, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let img = match &item.target {
            Target::App { id, name } => {
                let parsing = rf_win::apps::App { id: id.clone(), name: name.clone() }.parsing();
                self.images.update(cx, |c, cx| c.shell_icon(&parsing, crate::image_cache::ICON_PX, cx))
            }
            Target::Path { path, dir } => {
                let name = file_name(path);
                let e = rf_core::Entry::new(&name, if *dir { rf_core::EntryKind::Dir } else { rf_core::EntryKind::File }, 0, 0, 0, 0);
                let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
                self.images.update(cx, |c, cx| c.icon(&parent, &e, cx))
            }
            _ => None,
        };
        let fallback = match &item.target {
            Target::App { .. } => L::AppWindow,
            Target::Path { dir: true, .. } => L::Folder,
            Target::Path { .. } => L::File,
            Target::Url { .. } => L::Globe,
            Target::Web { .. } => L::Search,
        };
        div()
            .size(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .map(|d| match img {
                Some(i) => d.child(img_el(i)).into_any_element(),
                None => d.child(Icon::new(fallback).size(px(20.)).text_color(p.text_secondary)).into_any_element(),
            })
    }
}

fn img_el(i: Arc<RenderImage>) -> impl IntoElement {
    gpui_kit::img(i).size(px(28.)).object_fit(ObjectFit::Contain)
}

impl Render for QuickSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let n = self.items.len();
        let mut list = v_flex().px_2().pb_2();
        let mut last: Option<Section> = None;
        let items = self.items.clone();
        // the selected row's place among the list's children (section headers in between)
        let mut child = 0;
        let mut selected_child = 0;
        for (ix, item) in items.iter().enumerate() {
            if last != Some(item.section) {
                last = Some(item.section);
                list = list.child(div().px_2().pt_2().pb_1().text_xs().text_color(p.text_tertiary).child(section_title(item.section)));
                child += 1;
            }
            if ix == self.selected {
                selected_child = child;
            }
            child += 1;
            let sel = ix == self.selected;
            let title: SharedString = match &item.target {
                Target::Web { query } => format!("\u{201c}{query}\u{201d} {}", t!("için webde ara")).into(),
                _ => item.title.clone().into(),
            };
            let hint = if sel {
                match &item.target {
                    Target::Path { .. } => t!("Ctrl+Enter: klasörde göster"),
                    _ => "Enter",
                }
            } else {
                ""
            };
            let icon = self.icon(item, &p, cx);
            list = list.child(
                h_flex()
                    .id(("qs", ix))
                    .relative()
                    .h(px(ROW_H))
                    .px_2()
                    .gap_3()
                    .items_center()
                    .rounded(theme::RADIUS_SM)
                    .when(sel, |d| d.bg(p.selection).child(theme::selection_pill(&p)))
                    .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                    .child(icon)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(title))
                            .when(!item.detail.is_empty(), |d| d.child(div().text_xs().text_color(p.text_tertiary).truncate().child(item.detail.clone()))),
                    )
                    .when(!hint.is_empty(), |d| d.child(div().flex_shrink_0().text_xs().text_color(p.text_tertiary).child(hint)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        this.selected = ix;
                        let m = ev.modifiers();
                        this.activate(if m.control { How::ShowInFolder } else if m.shift { How::Preview } else { How::Open }, cx);
                    })),
            );
        }
        let width = WIDTH;
        let vh = window.viewport_size().height;
        let max_h = self.max_h;
        // what is left for the list under the search box, the notes and the key hints
        let notes = if self.index_building || self.index_off { 26. } else { 0. };
        let list_max = (max_h - 61. - 31. - notes).max(ROW_H * 2.);
        if std::mem::take(&mut self.reveal) {
            self.scroll.scroll_to_item(selected_child);
        }
        v_flex()
            .size_full()
            .text_color(p.text)
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                let m = &ev.keystroke.modifiers;
                match ev.keystroke.key.as_str() {
                    "down" if n > 0 => {
                        this.selected = (this.selected + 1) % n;
                        this.reveal = true;
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "up" if n > 0 => {
                        this.selected = (this.selected + n - 1) % n;
                        this.reveal = true;
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.activate(if m.control { How::ShowInFolder } else if m.shift { How::Preview } else { How::Open }, cx);
                    }
                    "escape" => {
                        cx.stop_propagation();
                        close_later(cx);
                    }
                    _ => {}
                }
            }))
            .child(
                div().relative().w_full().child(
                    v_flex()
                        .w_full()
                        .child(
                            h_flex()
                                .h(px(60.))
                                .px_4()
                                .gap_3()
                                .items_center()
                                .child(Icon::new(L::Search).size(px(20.)).text_color(p.text_secondary))
                                .child(div().flex_1().text_lg().child(Input::new(&self.input).appearance(false))),
                        )
                        .when(n > 0 || self.index_building, |d| d.child(div().h(px(1.)).w_full().bg(p.divider)))
                        .when(self.index_building, |d| {
                            d.child(div().px_4().pt_2().text_xs().text_color(p.text_tertiary).child(t!("Dosya dizini hazırlanıyor: bazı dosyalar henüz çıkmayabilir")))
                        })
                        .when(self.index_off && n > 0, |d| {
                            d.child(div().px_4().pt_2().text_xs().text_color(p.text_tertiary).child(t!("Dosya araması kapalı: Ayarlar'da ad dizinini açın")))
                        })
                        .child(div().id("qs-list").max_h(px(list_max)).overflow_y_scroll().track_scroll(&self.scroll).child(list))
                        .when(n > 0, |d| {
                            d.child(
                                h_flex()
                                    .h(px(30.))
                                    .px_4()
                                    .gap_4()
                                    .items_center()
                                    .border_t_1()
                                    .border_color(p.divider)
                                    .text_xs()
                                    .text_color(p.text_tertiary)
                                    .child(t!("Enter: aç"))
                                    .child(t!("Ctrl+Enter: klasörde göster"))
                                    .child(t!("Shift+Enter: önizle"))
                                    .child(t!("Esc: kapat")),
                            )
                        }),
                )
                // the window follows the content's height
                .child(
                    canvas(
                        move |b, window, _| {
                            let h = b.size.height.ceil().min(px(max_h));
                            if (f32::from(h) - f32::from(vh)).abs() > 1. {
                                window.on_next_frame(move |w, _| w.resize(size(px(width), h)));
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    // (not `super::*`: gpui's own `test` attribute would come with it)
    use super::{Bookmark, WebSearch, fold, parse_bookmarks, score, score_with, web_url};

    #[test]
    fn folding_and_scores() {
        assert_eq!(fold("Şarkılar İçin"), "sarkilar icin");
        // whole > start > word start > anywhere > initials
        let whole = score("Chrome", "chrome").unwrap();
        let start = score("Chrome Remote Desktop", "chrome").unwrap();
        let word = score("Google Chrome", "chrome").unwrap();
        let inside = score("Mychromeapp", "chrome").unwrap();
        let initials = score_with("Visual Studio Code", "vsc", true).unwrap();
        assert_eq!(score("Visual Studio Code", "vsc"), None);
        assert!(whole > start && start > word && word > inside && inside > initials, "{whole} {start} {word} {inside} {initials}");
        assert_eq!(score("Notepad", "xyz"), None);
        assert!(score("Şarkı listesi", "sarki").is_some());
    }

    #[test]
    fn bookmarks_from_browser_json() {
        let json = br#"{"checksum":"x","roots":{"bookmark_bar":{"type":"folder","name":"Bar","children":[
            {"type":"url","name":"GitHub","url":"https://github.com/"},
            {"type":"folder","name":"Dev","children":[{"type":"url","name":"","url":"https://docs.rs/"}]},
            {"type":"url","name":"Bookmarklet","url":"javascript:alert(1)"}]},
            "other":{"type":"folder","children":[]},"synced":{"type":"folder","children":[]}},"version":1}"#;
        let b = parse_bookmarks(json);
        assert_eq!(b, vec![Bookmark { name: "GitHub".into(), url: "https://github.com/".into() }, Bookmark { name: "https://docs.rs/".into(), url: "https://docs.rs/".into() }]);
    }

    #[test]
    fn web_search_urls() {
        assert_eq!(web_url(WebSearch::Google, "rust gpui"), "https://www.google.com/search?q=rust+gpui");
        assert_eq!(web_url(WebSearch::DuckDuckGo, "ç&="), "https://duckduckgo.com/?q=%C3%A7%26%3D");
    }
}
