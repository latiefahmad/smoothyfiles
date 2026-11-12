//! Keeps [`crate::nameindex::Index`]es of chosen roots (drives) ready for searching.
//!
//! [`start`] loads each root's saved index (usable at once), then catches up with what changed
//! while the app wasn't running: from the volume's change journal when it can tell (only the
//! changed folders are listed again, see [`crate::usn`]), else by walking the root again in the
//! background and swapping the fresh index in. A recursive watcher on
//! each root collects changed folders; once a second they are listed again
//! ([`crate::nameindex::Index::refresh_dirs`]). Indexes are saved after a build and at most every
//! ten minutes while they change. [`stop`] ends it all.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};

use crate::nameindex::Index;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing usable yet: the first walk is running.
    Building,
    /// Searchable (a saved index may be refreshing behind it).
    Ready,
    /// The root couldn't be indexed.
    Failed,
}

struct Slot {
    root: PathBuf,
    file: PathBuf,
    index: RwLock<Option<Index>>,
    state: AtomicU8,
    dirty: Mutex<HashSet<PathBuf>>,
    changed_since_save: AtomicBool,
    /// Followed through the volume's change journal (NTFS) instead of a recursive watcher.
    journaled: bool,
}

struct Service {
    slots: Vec<Arc<Slot>>,
    cancel: Arc<AtomicBool>,
    _watchers: Vec<notify::RecommendedWatcher>,
}

static SERVICE: OnceLock<Mutex<Option<Service>>> = OnceLock::new();

fn service() -> &'static Mutex<Option<Service>> {
    SERVICE.get_or_init(|| Mutex::new(None))
}

fn file_for(dir: &Path, root: &Path) -> PathBuf {
    let tag: String = root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    dir.join(format!("{}.rfidx", tag.trim_matches('_')))
}

/// Indexes `roots`, saving the indexes in `dir`. Replaces a running set.
pub fn start(roots: Vec<PathBuf>, dir: PathBuf) {
    stop();
    let _ = std::fs::create_dir_all(&dir);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut slots = Vec::new();
    let mut watchers = Vec::new();
    for root in roots {
        let journaled = crate::usn::query(&root).is_some();
        let slot = Arc::new(Slot {
            file: file_for(&dir, &root),
            root,
            index: RwLock::new(None),
            state: AtomicU8::new(State::Building as u8),
            dirty: Mutex::new(HashSet::new()),
            changed_since_save: AtomicBool::new(false),
            journaled,
        });
        // NTFS: the change journal says which folders changed, once a second (cheaper than a
        // watcher on a whole drive, which a build floods with events, and it can't overflow)
        if slot.journaled {
            slots.push(slot);
            continue;
        }
        // changed folders, collected from the start (a walk running meanwhile misses nothing:
        // the fresh index is refreshed with them once it's swapped in)
        let s2 = slot.clone();
        let ignore = dir.clone();
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                return;
            }
            let mut d = s2.dirty.lock();
            for p in ev.paths {
                if p.starts_with(&ignore) {
                    continue;
                }
                if let Some(parent) = p.parent() {
                    d.insert(parent.to_path_buf());
                }
            }
        }) {
            Ok(mut w) => {
                if let Err(e) = w.watch(&slot.root, RecursiveMode::Recursive) {
                    log::info!("indexer: no watch on {}: {e}", slot.root.display());
                } else {
                    watchers.push(w);
                }
            }
            Err(e) => log::info!("indexer: watcher: {e}"),
        }
        slots.push(slot);
    }
    for slot in &slots {
        let (slot, cancel) = (slot.clone(), cancel.clone());
        let _ = std::thread::Builder::new().name("rf-indexer".into()).spawn(move || run(slot, cancel));
    }
    *service().lock() = Some(Service { slots, cancel, _watchers: watchers });
}

/// Stops indexing (the indexes are dropped; saved files stay).
pub fn stop() {
    if let Some(s) = service().lock().take() {
        s.cancel.store(true, Ordering::Relaxed);
        for slot in &s.slots {
            save(slot);
        }
    }
}

fn save(slot: &Slot) {
    if !slot.changed_since_save.swap(false, Ordering::Relaxed) {
        // unchanged: only the journal position moves on (16 bytes, not the whole file); a file
        // that doesn't match the index is written whole below
        match slot.index.read().as_ref().map(|ix| ix.save_position(&slot.file)) {
            None | Some(Ok(true)) => return,
            Some(_) => {}
        }
    }
    // compacting changes ids: the only step that needs the index to itself
    {
        let mut guard = slot.index.write();
        if let Some(ix) = guard.as_mut() {
            if ix.len() > 0 && ix.len() - ix.live() > ix.len() / 5 {
                ix.compact();
            }
        }
    }
    // writing the file only reads it: searches go on meanwhile
    if let Some(ix) = slot.index.read().as_ref() {
        if let Err(e) = ix.save(&slot.file) {
            log::info!("indexer: saving {}: {e}", slot.file.display());
        }
    }
}

fn run(slot: Arc<Slot>, cancel: Arc<AtomicBool>) {
    // 1. the saved index, searchable at once
    match Index::load(&slot.file) {
        Ok(ix) if ix.root() == slot.root => {
            log::info!("indexer: {} loaded, {} entries", slot.root.display(), ix.live());
            *slot.index.write() = Some(ix);
            slot.state.store(State::Ready as u8, Ordering::Relaxed);
        }
        _ => {}
    }
    // 2. what changed while the app was closed: from the change journal, else a fresh walk
    let t = Instant::now();
    // (the folders are listed with no lock held: searches use the loaded index meanwhile)
    let caught = slot.index.read().is_some().then(|| crate::nameindex::catch_up_locked(&slot.index, &cancel));
    match caught {
        Some(Ok(n)) => {
            log::info!("indexer: {} caught up from the change journal in {:.2} s, {n} folders listed again", slot.root.display(), t.elapsed().as_secs_f32());
            slot.changed_since_save.store(true, Ordering::Relaxed);
            save(&slot);
            follow(&slot, &cancel);
            return;
        }
        Some(Err(e)) => {
            let saved = slot.index.read().as_ref().and_then(|ix| ix.journal());
            log::info!("indexer: {}: the change journal can't tell ({e:?}; saved at {saved:?}, journal now {:?}), walking", slot.root.display(), crate::usn::query(&slot.root));
        }
        None => {}
    }
    let fresh = Index::build(&slot.root, &cancel);
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    log::info!("indexer: {} walked in {:.1} s, {} entries, {} MB", slot.root.display(), t.elapsed().as_secs_f32(), fresh.live(), fresh.memory() >> 20);
    if fresh.is_empty() && slot.index.read().is_none() {
        slot.state.store(State::Failed as u8, Ordering::Relaxed);
    } else {
        *slot.index.write() = Some(fresh);
        slot.state.store(State::Ready as u8, Ordering::Relaxed);
        slot.changed_since_save.store(true, Ordering::Relaxed);
        save(&slot);
    }
    follow(&slot, &cancel);
}

/// 3. keeps up with changes until cancelled.
fn follow(slot: &Slot, cancel: &AtomicBool) {
    if slot.journaled {
        return follow_journal(slot, cancel);
    }
    let mut last_save = Instant::now();
    // The journal position one round ago: by now the watcher has reported every change
    // before it and the folders were listed again, so the saved index is current up to there
    // (a start reads from it; reading a little twice costs nothing).
    let mut seen = crate::usn::query(&slot.root).map(|j| j.next);
    while !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let now = crate::usn::query(&slot.root).map(|j| j.next);
        let dirs: Vec<PathBuf> = slot.dirty.lock().drain().collect();
        if !dirs.is_empty() {
            crate::nameindex::refresh_locked(&slot.index, &dirs, cancel);
            slot.changed_since_save.store(true, Ordering::Relaxed);
        }
        if let Some(next) = seen {
            if let Some(ix) = slot.index.write().as_mut() {
                ix.set_journal_next(next);
            }
        }
        seen = now;
        // the position alone every minute (a crash or a shutdown then costs little re-reading),
        // the whole index at most every ten minutes when it changed
        if !slot.changed_since_save.load(Ordering::Relaxed) && last_save.elapsed() > Duration::from_secs(60) {
            save(slot);
            last_save = Instant::now();
        } else if last_save.elapsed() > Duration::from_secs(600) {
            save(&slot);
            last_save = Instant::now();
        }
    }
}

/// Searches the index whose root holds `under`: `None` when no ready index covers it (search
/// the disk then).
pub fn search(pattern: &str, under: &Path, include_hidden: bool, max: usize) -> Option<Vec<PathBuf>> {
    let slot = {
        let g = service().lock();
        let s = g.as_ref()?;
        s.slots.iter().find(|s| under.starts_with(&s.root) || under.to_string_lossy().to_lowercase().starts_with(&s.root.to_string_lossy().to_lowercase())).cloned()?
    };
    if slot.state.load(Ordering::Relaxed) != State::Ready as u8 {
        return None;
    }
    let ix = slot.index.read();
    ix.as_ref()?.search(pattern, Some(under), include_hidden, max)
}

/// [`search`] with the best matches first ([`Index::search_ranked`]): `rank` scores a name.
pub fn search_ranked(pattern: &str, under: &Path, include_hidden: bool, max: usize, rank: &(dyn Fn(&str) -> Option<i32> + Sync)) -> Option<Vec<(i32, PathBuf)>> {
    let slot = {
        let g = service().lock();
        let s = g.as_ref()?;
        s.slots.iter().find(|s| under.starts_with(&s.root) || under.to_string_lossy().to_lowercase().starts_with(&s.root.to_string_lossy().to_lowercase())).cloned()?
    };
    if slot.state.load(Ordering::Relaxed) != State::Ready as u8 {
        return None;
    }
    let ix = slot.index.read();
    ix.as_ref()?.search_ranked(pattern, Some(under), include_hidden, max, rank)
}

/// (root, state, live entries, bytes in memory) per root.
pub fn status() -> Vec<(PathBuf, State, usize, usize)> {
    let g = service().lock();
    let Some(s) = g.as_ref() else { return Vec::new() };
    s.slots
        .iter()
        .map(|slot| {
            let state = match slot.state.load(Ordering::Relaxed) {
                x if x == State::Ready as u8 => State::Ready,
                x if x == State::Failed as u8 => State::Failed,
                _ => State::Building,
            };
            let (live, mem) = slot.index.try_read().and_then(|i| i.as_ref().map(|i| (i.live(), i.memory()))).unwrap_or((0, 0));
            (slot.root.clone(), state, live, mem)
        })
        .collect()
}

/// The drives worth indexing: local, fixed, NTFS or ReFS. Cloud drives (Google Drive, a
/// mounted share) and removable disks stay out: walking them would download files or wake disks.
pub fn local_roots() -> Vec<PathBuf> {
    crate::drives::list_drives()
        .into_iter()
        .filter(|d| d.kind == crate::drives::DriveKind::Fixed && matches!(d.fs.to_ascii_uppercase().as_str(), "NTFS" | "REFS"))
        .map(|d| d.root)
        .collect()
}

pub fn running() -> bool {
    service().lock().is_some()
}

/// [`follow`] for a volume with a change journal: once a second the folders it names are listed
/// again ([`Index::catch_up`]). When it can't tell (records dropped while the app was suspended,
/// a reset journal, a flood of changes), the root is walked again.
fn follow_journal(slot: &Slot, cancel: &AtomicBool) {
    let mut last_save = Instant::now();
    let mut unreadable = 0;
    while !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let r = crate::nameindex::catch_up_locked(&slot.index, cancel);
        match r {
            Ok(0) => unreadable = 0,
            Ok(_) => {
                unreadable = 0;
                slot.changed_since_save.store(true, Ordering::Relaxed);
            }
            Err(crate::usn::Gap::Unreadable) if unreadable < 10 => unreadable += 1,
            Err(e) => {
                log::info!("indexer: {}: lost the change journal ({e:?}), walking again", slot.root.display());
                let fresh = Index::build(&slot.root, cancel);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                *slot.index.write() = Some(fresh);
                slot.changed_since_save.store(true, Ordering::Relaxed);
                unreadable = 0;
            }
        }
        if !slot.changed_since_save.load(Ordering::Relaxed) && last_save.elapsed() > Duration::from_secs(60) {
            save(slot);
            last_save = Instant::now();
        } else if last_save.elapsed() > Duration::from_secs(600) {
            save(slot);
            last_save = Instant::now();
        }
    }
}
