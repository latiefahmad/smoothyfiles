//! Tags as a GPUI global, persisted to %APPDATA%\smoothyfiles\tags.json.

use std::path::PathBuf;

use gpui_kit::*;
use rf_core::tags::TagStore;

pub struct Tags(pub TagStore);
impl Global for Tags {}

fn path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("tags.json"))
}

pub fn load() -> Tags {
    Tags(path().map(|p| TagStore::load(&p)).unwrap_or_default())
}

pub fn update(cx: &mut App, f: impl FnOnce(&mut TagStore)) {
    cx.update_global::<Tags, _>(|t, _| f(&mut t.0));
    let snapshot = cx.global::<Tags>().0.clone();
    std::thread::spawn(move || {
        if let Some(p) = path() {
            if let Err(e) = snapshot.save(&p) {
                log::warn!("tags save: {e}");
            }
        }
    });
    cx.refresh_windows();
}

pub fn color(c: u32) -> Hsla {
    rgb(c).into()
}
