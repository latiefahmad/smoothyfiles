//! Recursive filename search ("in this folder and subfolders").
//!
//! Directories are walked in parallel on a dedicated rayon pool whose threads run in background
//! priority mode, so the search never competes with the user's foreground work. Results are
//! streamed in batches to the calling thread (which owns `on_batch` and `cancelled`).
//! Blocking: call off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, Sender, unbounded};
use rf_core::{Entry, attrs};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, GetLastError};
use windows::Win32::Storage::FileSystem::{
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    WIN32_FIND_DATAW,
};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
use windows::core::PCWSTR;

use crate::listing::to_entry;
use crate::wide::long_path;

const BATCH: usize = 200;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub root: PathBuf,
    /// Case-insensitive substring, or a glob when it contains `*` / `?` (matched against the whole name).
    pub pattern: String,
    pub include_hidden: bool,
    pub max_results: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub entry: Entry,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub dirs_scanned: usize,
    pub entries_scanned: usize,
    pub hits: usize,
    /// Directories that could not be read (access denied etc.).
    pub skipped_dirs: usize,
    /// Stopped because `max_results` was reached.
    pub truncated: bool,
    pub cancelled: bool,
}

/// Lowercases and drops the combining dot above (U+0307) that `"İ".to_lowercase()` yields, so
/// "İ" and "i" compare equal.
/// Case-folded for matching. Dotless ı counts as i (and İ, lowered to i + combining dot, loses
/// the dot), so "IŞIK", "ışık" and "Işık" find each other, as Windows' own comparison does.
/// The other Turkish letters count as their plain ones too (ş = s, ğ = g, ü = u, ö = o, ç = c,
/// and the circumflexed â î û): "sarki" finds "Şarkı", as people type on a keyboard without them.
fn fold(s: &str) -> Vec<char> {
    s.to_lowercase()
        .chars()
        .filter(|&c| c != '\u{307}')
        .map(|c| match c {
            'ı' | 'î' => 'i',
            'ş' => 's',
            'ğ' => 'g',
            'ü' | 'û' => 'u',
            'ö' => 'o',
            'ç' => 'c',
            'â' => 'a',
            c => c,
        })
        .collect()
}

pub(crate) enum Matcher {
    All,
    Substr(Vec<char>),
    Glob(Vec<char>),
}

impl Matcher {
    pub(crate) fn new(pattern: &str) -> Self {
        let p = fold(pattern.trim());
        if p.is_empty() {
            Matcher::All
        } else if p.iter().any(|&c| c == '*' || c == '?') {
            Matcher::Glob(p)
        } else {
            Matcher::Substr(p)
        }
    }

    pub(crate) fn is_match(&self, name: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Substr(p) => {
                let n = fold(name);
                n.len() >= p.len() && n.windows(p.len()).any(|w| w == p.as_slice())
            }
            Matcher::Glob(p) => glob_match(p, &fold(name)),
        }
    }
}

/// `*` = any run (including empty), `?` = exactly one char. Iterative with backtracking.
fn glob_match(p: &[char], n: &[char]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while ni < n.len() {
        if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Enumerates `dir`, calling `f` per raw find record (including "." and ".."). Err = Win32 error code.
pub(crate) fn for_each_raw(dir: &Path, mut f: impl FnMut(&WIN32_FIND_DATAW)) -> Result<(), u32> {
    let mut pattern = long_path(dir);
    pattern.pop();
    if pattern.last() != Some(&(b'\\' as u16)) {
        pattern.push(b'\\' as u16);
    }
    pattern.push(b'*' as u16);
    pattern.push(0);
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let Ok(handle) = handle else {
        let e = unsafe { GetLastError() };
        return if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(e.0) };
    };
    let res = loop {
        f(&data);
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            let e = unsafe { GetLastError() };
            break if e == ERROR_NO_MORE_FILES { Ok(()) } else { Err(e.0) };
        }
    };
    unsafe {
        let _ = FindClose(handle);
    }
    res
}

struct Shared<'a> {
    matcher: &'a Matcher,
    include_hidden: bool,
    max: usize,
    stop: &'a AtomicBool,
    reserved: &'a AtomicUsize,
    dirs: &'a AtomicUsize,
    entries: &'a AtomicUsize,
    skipped: &'a AtomicUsize,
    tx: &'a Sender<Vec<SearchHit>>,
}

fn walk<'s>(sc: &rayon::Scope<'s>, sh: &'s Shared<'s>, dir: PathBuf) {
    if sh.stop.load(Ordering::Relaxed) {
        return;
    }
    let mut batch: Vec<SearchHit> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut count = 0usize;
    let r = for_each_raw(&dir, |d| {
        if sh.stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(entry) = to_entry(d) else { return };
        count += 1;
        let a = entry.attrs;
        if a & attrs::HIDDEN != 0 && !sh.include_hidden {
            return;
        }
        if entry.is_dir() && a & attrs::REPARSE_POINT == 0 {
            subdirs.push(dir.join(&*entry.name));
        }
        if sh.matcher.is_match(&entry.name) {
            if sh.reserved.fetch_add(1, Ordering::Relaxed) >= sh.max {
                sh.stop.store(true, Ordering::Relaxed);
                return;
            }
            batch.push(SearchHit { path: dir.join(&*entry.name), entry });
            if batch.len() >= BATCH {
                let _ = sh.tx.send(std::mem::take(&mut batch));
            }
        }
    });
    sh.dirs.fetch_add(1, Ordering::Relaxed);
    sh.entries.fetch_add(count, Ordering::Relaxed);
    if let Err(code) = r {
        if code != ERROR_ACCESS_DENIED.0 {
            log::debug!("search: {} failed ({code})", dir.display());
        }
        sh.skipped.fetch_add(1, Ordering::Relaxed);
    }
    if !batch.is_empty() {
        let _ = sh.tx.send(batch);
    }
    for sub in subdirs {
        sc.spawn(move |sc| walk(sc, sh, sub));
    }
}

/// Searches `q.root` recursively. `on_batch` receives ~200 hits at a time on the calling thread;
/// `cancelled` is polled there too (every few milliseconds). Hidden entries are skipped (and
/// hidden directories not entered) unless `include_hidden`; reparse points (junctions, symlinks)
/// are listed but never followed.
pub fn search(
    q: &SearchQuery,
    cancelled: &dyn Fn() -> bool,
    on_batch: &mut dyn FnMut(Vec<SearchHit>),
) -> anyhow::Result<SearchStats> {
    anyhow::ensure!(q.root.is_dir(), "Klasör bulunamadı: {}", q.root.display());
    let matcher = Matcher::new(&q.pattern);
    let max = q.max_results;
    let stop = AtomicBool::new(false);
    let reserved = AtomicUsize::new(0);
    let (dirs, entries, skipped) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let (tx, rx) = unbounded::<Vec<SearchHit>>();

    // as many as the folder size engine (measured there: 8 is the sweet spot on NVMe)
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("rf-search-{i}"))
        .start_handler(|_| unsafe {
            // Below-normal CPU priority (the UI thread always wins). Not background mode: that
            // also drops the I/O priority to the floor, and the walk crawled at ~30 000
            // entries a second.
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        })
        .build()?;

    let shared = Shared {
        matcher: &matcher,
        include_hidden: q.include_hidden,
        max,
        stop: &stop,
        reserved: &reserved,
        dirs: &dirs,
        entries: &entries,
        skipped: &skipped,
        tx: &tx,
    };
    let mut hits = 0usize;
    let mut was_cancelled = false;
    let mut emit = |mut b: Vec<SearchHit>, was_cancelled: bool| {
        if was_cancelled || hits >= max {
            return;
        }
        b.truncate(max - hits);
        hits += b.len();
        on_batch(b);
    };
    std::thread::scope(|ts| {
        let walker = ts.spawn(|| {
            if max > 0 {
                let sh = &shared;
                pool.scope(|sc| walk(sc, sh, q.root.clone()));
            }
        });
        loop {
            if !was_cancelled && cancelled() {
                was_cancelled = true;
                stop.store(true, Ordering::Relaxed);
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(b) => emit(b, was_cancelled),
                Err(RecvTimeoutError::Timeout) => {
                    if walker.is_finished() {
                        while let Ok(b) = rx.try_recv() {
                            emit(b, was_cancelled);
                        }
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = walker.join();
    });

    Ok(SearchStats {
        dirs_scanned: dirs.load(Ordering::Relaxed),
        entries_scanned: entries.load(Ordering::Relaxed),
        hits,
        skipped_dirs: skipped.load(Ordering::Relaxed),
        truncated: !was_cancelled && reserved.load(Ordering::Relaxed) > max,
        cancelled: was_cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rf-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("d")).unwrap();
        for f in ["Report.TXT", "a/report2.txt", "a/b/c/deep_report.md", "a/b/notes.txt", "d/İstanbul.txt", "d/other.bin"] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        root
    }

    fn run(root: &Path, pat: &str, max: usize) -> (Vec<String>, SearchStats) {
        let q = SearchQuery { root: root.to_path_buf(), pattern: pat.into(), include_hidden: false, max_results: max };
        let mut names = Vec::new();
        let st = search(&q, &|| false, &mut |b| names.extend(b.into_iter().map(|h| h.entry.name.to_string()))).unwrap();
        names.sort();
        (names, st)
    }

    #[test]
    fn substring_case_insensitive_nested() {
        let root = tree("sub");
        let (n, st) = run(&root, "REPORT", 100);
        assert_eq!(n, vec!["Report.TXT", "deep_report.md", "report2.txt"]);
        assert_eq!(st.hits, 3);
        assert!(st.dirs_scanned >= 5 && !st.truncated && !st.cancelled);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wildcards() {
        let root = tree("glob");
        assert_eq!(run(&root, "*.txt", 100).0, vec!["Report.TXT", "notes.txt", "report2.txt", "İstanbul.txt"]);
        assert_eq!(run(&root, "report?.txt", 100).0, vec!["report2.txt"]);
        assert_eq!(run(&root, "*", 100).0.len(), 6 + 4); // files + dirs a, b, c, d
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn turkish_case_folding() {
        let root = tree("tr");
        assert_eq!(run(&root, "istanbul", 100).0, vec!["İstanbul.txt"]);
        assert_eq!(run(&root, "İSTANBUL", 100).0, vec!["İstanbul.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
        // typed without the Turkish letters, and the other way round
        assert!(Matcher::new("sarki").is_match("Şarkı listesi.mp3"));
        assert!(Matcher::new("ÖĞRENCİ").is_match("ogrenci.xlsx"));
        assert!(Matcher::new("*cocuk*").is_match("Çocuk.jpg"));
        assert!(!Matcher::new("sarki").is_match("saray.txt"));
    }

    #[test]
    fn max_results_and_cancel() {
        let root = tree("max");
        let (n, st) = run(&root, "*.txt", 2);
        assert_eq!(n.len(), 2);
        assert!(st.truncated);
        let q = SearchQuery { root: root.clone(), pattern: "*".into(), include_hidden: false, max_results: 100 };
        let mut got = 0;
        let st = search(&q, &|| true, &mut |b| got += b.len()).unwrap();
        assert!(st.cancelled);
        assert_eq!(got, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn streams_in_batches() {
        let root = std::env::temp_dir().join(format!("rf-search-big-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..450 {
            std::fs::write(root.join(format!("f{i}.dat")), b"").unwrap();
        }
        let q = SearchQuery { root: root.clone(), pattern: ".dat".into(), include_hidden: false, max_results: 10_000 };
        let mut sizes = Vec::new();
        search(&q, &|| false, &mut |b| sizes.push(b.len())).unwrap();
        assert_eq!(sizes.iter().sum::<usize>(), 450);
        assert!(sizes.iter().all(|&s| s <= BATCH) && sizes.len() >= 3);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn glob_edge_cases() {
        let g = |p: &str, n: &str| glob_match(&fold(p), &fold(n));
        assert!(g("a*b*c", "aXXbYYc"));
        assert!(!g("a*b*c", "aXXbYY"));
        assert!(g("*", ""));
        assert!(g("?", "x") && !g("?", ""));
    }
}
