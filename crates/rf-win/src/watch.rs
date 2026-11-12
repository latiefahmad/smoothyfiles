//! Directory change notifications (ReadDirectoryChangesW via `notify`), debounced.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Keeps the watch alive; drop to stop.
pub struct DirWatch {
    _w: RecommendedWatcher,
}

/// What changed in a watched folder during one burst.
#[derive(Debug, Default, Clone)]
pub struct Changes {
    /// Names (directly in the folder) that were added, removed, renamed from or to, or changed.
    pub names: Vec<String>,
    /// The names can't be told (the system dropped events, too many changed, the folder itself
    /// changed): list the folder again.
    pub rescan: bool,
}

/// More changed names than this in one burst: listing the folder again is cheaper.
const MAX_NAMES: usize = 256;

/// Calls `on_change` (from a background thread) once changes in `dir` pause for `debounce`, and
/// during a burst that never pauses (files renamed in the background without end) at least every
/// `debounce` × 5, so the view never falls behind for long.
pub fn watch_dir(dir: &Path, debounce: Duration, on_change: impl Fn() + Send + Sync + 'static) -> anyhow::Result<DirWatch> {
    watch_dir_changes(dir, debounce, move |_| on_change())
}

/// [`watch_dir`] telling what changed: the names, or that the folder must be listed again.
pub fn watch_dir_changes(dir: &Path, debounce: Duration, on_change: impl Fn(Changes) + Send + Sync + 'static) -> anyhow::Result<DirWatch> {
    enum Ev {
        Paths(Vec<std::path::PathBuf>),
        Rescan,
    }
    let (tx, rx) = crossbeam_channel::unbounded::<Ev>();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                let _ = tx.send(if ev.need_rescan() || ev.paths.is_empty() { Ev::Rescan } else { Ev::Paths(ev.paths) });
            }
        }
        Err(_) => {
            let _ = tx.send(Ev::Rescan);
        }
    })?;
    let watched = dir.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_lowercase();
    let name_of = move |p: &Path| -> Option<String> {
        // a direct child of the folder (anything else, the folder itself included: rescan)
        let parent = p.parent()?.to_string_lossy().trim_start_matches(r"\\?\").trim_end_matches('\\').to_lowercase();
        if parent != watched {
            return None;
        }
        Some(p.file_name()?.to_str()?.to_string())
    };
    // notify opens the directory without the `\\?\` prefix: past MAX_PATH that fails and the
    // view would silently stop updating. Give it the long form there.
    let target = if dir.as_os_str().len() >= 240 {
        use std::os::windows::ffi::OsStringExt;
        let mut long = crate::wide::long_path(dir);
        long.pop(); // NUL
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&long))
    } else {
        dir.to_path_buf()
    };
    w.watch(&target, RecursiveMode::NonRecursive)?;
    let on_change = Arc::new(on_change);
    let max_wait = debounce * 5;
    std::thread::Builder::new().name("rf-watch".into()).spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut ch = Changes::default();
            let mut seen = std::collections::HashSet::new();
            let mut take = |ev: Ev, ch: &mut Changes| match ev {
                Ev::Rescan => ch.rescan = true,
                Ev::Paths(ps) => {
                    for p in ps {
                        match name_of(&p) {
                            Some(n) if !ch.rescan => {
                                if seen.insert(n.clone()) {
                                    ch.names.push(n);
                                }
                                if ch.names.len() > MAX_NAMES {
                                    ch.rescan = true;
                                }
                            }
                            Some(_) => {}
                            None => ch.rescan = true,
                        }
                    }
                }
            };
            take(first, &mut ch);
            // swallow the burst, but not for longer than max_wait
            let burst = std::time::Instant::now();
            loop {
                let left = max_wait.saturating_sub(burst.elapsed());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(debounce.min(left)) {
                    Ok(ev) => take(ev, &mut ch),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
            }
            if ch.rescan {
                ch.names.clear();
            }
            on_change(ch);
        }
    })?;
    Ok(DirWatch { _w: w })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// The changed names come with the report: an added, a renamed and a deleted file.
    #[test]
    fn reports_the_changed_names() {
        let dir = std::env::temp_dir().join(format!("rf-watch-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("eski.txt"), b"x").unwrap();
        std::fs::write(dir.join("silinecek.txt"), b"x").unwrap();
        let got = Arc::new(std::sync::Mutex::new(Vec::<Changes>::new()));
        let g2 = got.clone();
        let _w = watch_dir_changes(&dir, Duration::from_millis(100), move |c| g2.lock().unwrap().push(c)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(dir.join("yeni şarkı.txt"), b"x").unwrap();
        std::fs::rename(dir.join("eski.txt"), dir.join("Yeni Ad.txt")).unwrap();
        std::fs::remove_file(dir.join("silinecek.txt")).unwrap();
        let start = Instant::now();
        let mut names = std::collections::BTreeSet::new();
        while start.elapsed() < Duration::from_secs(3) {
            for c in got.lock().unwrap().iter() {
                assert!(!c.rescan, "{c:?}");
                names.extend(c.names.iter().cloned());
            }
            if names.len() >= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let want: std::collections::BTreeSet<String> = ["yeni şarkı.txt", "eski.txt", "Yeni Ad.txt", "silinecek.txt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(names, want);
        // and each reads back as listed: present or gone
        assert!(crate::stat_entry(&dir, "Yeni Ad.txt").unwrap().is_some());
        assert!(crate::stat_entry(&dir, "silinecek.txt").unwrap().is_none());
        assert_eq!(&*crate::stat_entry(&dir, "yeni ad.txt").unwrap().unwrap().name, "Yeni Ad.txt");
        drop(_w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_burst_without_pause_still_reports() {
        let dir = std::env::temp_dir().join(format!("rf-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let _w = watch_dir(&dir, Duration::from_millis(150), move || {
            c2.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        // a change every 30 ms for 2 s: never 150 ms of quiet
        let start = Instant::now();
        let mut i = 0;
        while start.elapsed() < Duration::from_secs(2) {
            std::fs::write(dir.join(format!("f{}.txt", i % 50)), format!("{i}")).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(30));
        }
        let during = calls.load(Ordering::Relaxed);
        assert!(during >= 2, "reported {during} times during a 2 s burst");
        // and once more after it settles
        std::thread::sleep(Duration::from_millis(400));
        assert!(calls.load(Ordering::Relaxed) > during);
        drop(_w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
