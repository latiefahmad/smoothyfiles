//! Per-folder layout memory (Settings → Layout → "Sync layout across folders" off): each
//! folder remembers its view, sort and grouping, like Files. Stored in
//! `%APPDATA%\smoothyfiles\layouts.json`, written in the background after a change.

use std::collections::HashMap;
use std::path::Path;

use gpui_kit::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderLayout {
    /// 0 details, 1 grid, 2 columns
    pub view: u8,
    pub sort: rf_core::SortKey,
    pub desc: bool,
    pub dirs_first: bool,
    #[serde(default)]
    pub files_first: bool,
    /// "Size" slider: Details row height / grid tile size (indices).
    #[serde(default = "default_row")]
    pub row_size: u8,
    #[serde(default = "default_grid")]
    pub grid_size: u8,
    /// 0 none, 1 name, 2 type, 3 modified, 4 size
    pub group: u8,
}

fn default_row() -> u8 {
    1
}
fn default_grid() -> u8 {
    2
}

#[derive(Default)]
struct Layouts {
    map: HashMap<String, FolderLayout>,
    loaded: bool,
    dirty_gen: u64,
}
impl Global for Layouts {}

const MAX: usize = 4000;

fn file() -> Option<std::path::PathBuf> {
    crate::settings::app_dir().map(|d| d.join("layouts.json"))
}

fn key(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

fn store(cx: &mut App) -> &mut Layouts {
    if !cx.has_global::<Layouts>() {
        cx.set_global(Layouts::default());
    }
    let l = cx.global_mut::<Layouts>();
    if !l.loaded {
        l.loaded = true;
        if let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) {
            l.map = serde_json::from_str(&text).unwrap_or_default();
        }
    }
    l
}

pub fn get(path: &Path, cx: &mut App) -> Option<FolderLayout> {
    store(cx).map.get(&key(path)).copied()
}

pub fn set(path: &Path, layout: FolderLayout, cx: &mut App) {
    let s = store(cx);
    if s.map.get(&key(path)) == Some(&layout) {
        return;
    }
    if s.map.len() >= MAX {
        // forget an arbitrary half; layouts are a convenience, not data
        let drop: Vec<String> = s.map.keys().take(MAX / 2).cloned().collect();
        for k in drop {
            s.map.remove(&k);
        }
    }
    s.map.insert(key(path), layout);
    s.dirty_gen += 1;
    let gen_ = s.dirty_gen;
    // coalesce bursts (sort clicks) into one write
    cx.spawn(async move |cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
        let _ = cx.update(|cx| {
            let s = store(cx);
            if s.dirty_gen != gen_ {
                return;
            }
            let Ok(text) = serde_json::to_string(&s.map) else { return };
            if let Some(f) = file() {
                std::thread::spawn(move || {
                    if let Some(d) = f.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let tmp = f.with_extension("json.tmp");
                    if std::fs::write(&tmp, text).is_ok() {
                        let _ = std::fs::rename(&tmp, &f);
                    }
                });
            }
        });
    })
    .detach();
}
