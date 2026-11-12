//! The speed test in Settings: smoothyfiles against the other file managers on this machine, and
//! its own engines against Windows'. Blocking: run [`run`] on a background thread.
//!
//! Test folders live in `<app dir>/bench` and are kept between runs (making 100 000 files takes a
//! while); copies and extractions go to a scratch folder that is emptied after each run.
//! smoothyfiles is measured as a fresh process with its own settings folder, the way the others
//! start: the user's settings are neither read nor changed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rf_win::bench::{App, AppKind, Machine, OpenResult};

/// Each app opens each folder this many times (after one warm-up run that isn't counted).
pub const RUNS: usize = 3;
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenTest {
    /// An empty folder: how long the app takes to start.
    Start,
    Files1k,
    Files10k,
    Files100k,
    /// 2 000 pictures: thumbnails.
    Images,
}

impl OpenTest {
    pub const ALL: [OpenTest; 5] = [OpenTest::Start, OpenTest::Files1k, OpenTest::Files10k, OpenTest::Files100k, OpenTest::Images];

    /// The test folder's name (also what the window titles are matched by).
    pub fn folder(self) -> &'static str {
        match self {
            OpenTest::Start => "rf-bench empty",
            OpenTest::Files1k => "rf-bench 1000",
            OpenTest::Files10k => "rf-bench 10000",
            OpenTest::Files100k => "rf-bench 100000",
            OpenTest::Images => "rf-bench images",
        }
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub struct Plan {
    /// smoothyfiles first, then the apps to compare with.
    pub apps: Vec<App>,
    pub open: Vec<OpenTest>,
    pub copy: bool,
    pub engines: bool,
}

/// One app's runs of one open test.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppRuns {
    pub app: AppKind,
    pub runs: Vec<OpenResult>,
    /// Why it couldn't be measured, if it couldn't.
    pub error: Option<String>,
}

impl AppRuns {
    /// The run with the median settle time.
    pub fn median(&self) -> Option<&OpenResult> {
        let mut v: Vec<&OpenResult> = self.runs.iter().collect();
        v.sort_by(|a, b| a.settled_ms.total_cmp(&b.settled_ms));
        v.get(v.len() / 2).copied()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenResults {
    pub test: OpenTest,
    pub apps: Vec<AppRuns>,
}

/// Ours against Windows' engine, in ms (median of the runs); `None` when it failed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Versus {
    pub ours_ms: Option<f64>,
    pub windows_ms: Option<f64>,
    pub bytes: u64,
    pub files: u64,
    /// CPU time of each side (medians, ms) and the private memory it took on top (MB). Results
    /// saved before these were measured have none.
    #[serde(default)]
    pub ours_cpu_ms: Option<f64>,
    #[serde(default)]
    pub windows_cpu_ms: Option<f64>,
    #[serde(default)]
    pub ours_mb: Option<f64>,
    #[serde(default)]
    pub windows_mb: Option<f64>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Engines {
    /// 10 000 small files in 100 folders.
    pub copy_small: Option<Versus>,
    /// One 1 GB file.
    pub copy_big: Option<Versus>,
    /// Extracting a ZIP of 10 000 files: rarchive against Windows' compressed folders.
    pub unzip: Option<Versus>,
    /// Searching the test folders by name (ms, entries looked at).
    pub search_ms: Option<f64>,
    pub search_entries: u64,
    /// The search's CPU time (ms) and extra memory (MB).
    #[serde(default)]
    pub search_cpu_ms: Option<f64>,
    #[serde(default)]
    pub search_mb: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Results {
    /// Unix ms.
    pub when: i64,
    pub version: String,
    pub machine: Machine,
    pub open: Vec<OpenResults>,
    pub engines: Engines,
}

/// Progress for the UI.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Making the test folders (what, done 0..1).
    Preparing(String, f32),
    /// Step `i` of `n`.
    Step { i: usize, n: usize, label: String },
}

/// `%LOCALAPPDATA%/smoothyfiles/bench`: local, never the roaming settings folder (a roaming
/// profile would carry 100 000 test files around).
pub fn bench_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("smoothyfiles").join("bench")
}

/// smoothyfiles itself, started fresh with its own settings folder.
pub fn smoothyfiles_app() -> App {
    let exe = std::env::current_exe().map(|p| p.with_file_name("smoothyfiles.exe")).unwrap_or_default();
    let cfg = bench_dir().join("smoothyfiles-settings");
    App {
        kind: AppKind::Smoothyfiles,
        exe,
        env: vec![
            ("SMOOTHYFILES_MULTI_INSTANCE".into(), "1".into()),
            ("SMOOTHYFILES_CONFIG_DIR".into(), cfg.to_string_lossy().into_owned()),
            ("SMOOTHYFILES_MUTE".into(), "1".into()),
        ],
    }
}

// ------------------------------------------------------------------ test folders

const EXTS: [&str; 10] = ["txt", "pdf", "docx", "zip", "mp3", "rs", "md", "json", "csv", "log"];

fn make_files(dir: &Path, n: usize, report: &dyn Fn(f32)) -> std::io::Result<()> {
    let done = dir.join(".rf-bench-done");
    if done.exists() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let body = vec![b'x'; 600];
    for i in 0..n {
        // small enough to stay inside the file record: no clusters used
        let len = (i * 37) % 600;
        std::fs::write(dir.join(format!("file {i:06}.{}", EXTS[i % EXTS.len()])), &body[..len])?;
        if i % 2000 == 0 {
            report(i as f32 / n as f32);
        }
    }
    hide(&done)?;
    Ok(())
}

/// The "done" marker, hidden so it doesn't show in the listings.
fn hide(p: &Path) -> std::io::Result<()> {
    std::fs::write(p, b"")?;
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let _ = windows::Win32::Storage::FileSystem::SetFileAttributesW(
            windows::core::PCWSTR(w.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN,
        );
    }
    Ok(())
}

fn make_images(dir: &Path, n: usize, report: &dyn Fn(f32)) -> anyhow::Result<()> {
    let done = dir.join(".rf-bench-done");
    if done.exists() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    for i in 0..n {
        let (w, h) = (320u32, 240u32);
        let hue = (i * 47 % 360) as f32;
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let t = (x + y) as f32 / (w + h) as f32;
            let c = |o: f32| (((hue + o) / 360.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5) * 200.0 * (0.5 + t * 0.5) + 30.0;
            image::Rgb([c(0.0) as u8, c(120.0) as u8, c(240.0) as u8])
        });
        img.save(dir.join(format!("picture {i:04}.png")))?;
        if i % 50 == 0 {
            report(i as f32 / n as f32);
        }
    }
    hide(&done)?;
    Ok(())
}

/// 10 000 files of 4 KB in 100 folders, and one 1 GB file.
fn make_copy_sources(root: &Path, report: &dyn Fn(f32)) -> std::io::Result<(PathBuf, PathBuf)> {
    let small = root.join("copy small");
    let big = root.join("copy big.bin");
    let done = root.join(".rf-bench-copy-done");
    if done.exists() && big.exists() {
        return Ok((small, big));
    }
    let _ = std::fs::remove_dir_all(&small);
    for d in 0..100 {
        let dir = small.join(format!("folder {d:03}"));
        std::fs::create_dir_all(&dir)?;
        for f in 0..100u32 {
            let body: Vec<u8> = (0..4096u32).map(|i| ((i + f * 7 + d) % 251) as u8).collect();
            std::fs::write(dir.join(format!("file {f:03}.dat")), body)?;
        }
        report(d as f32 / 200.0);
    }
    use std::io::Write;
    let chunk: Vec<u8> = (0..(4u32 << 20)).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let mut f = std::io::BufWriter::new(std::fs::File::create(&big)?);
    for k in 0..256 {
        f.write_all(&chunk)?;
        if k % 16 == 0 {
            report(0.5 + k as f32 / 512.0);
        }
    }
    f.flush()?;
    hide(&done)?;
    Ok((small, big))
}

/// Writes the benchmark instance's settings: nothing that would stay running or reach out.
fn prepare_smoothyfiles_settings() -> std::io::Result<()> {
    let cfg = bench_dir().join("smoothyfiles-settings");
    std::fs::create_dir_all(&cfg)?;
    std::fs::write(
        cfg.join("settings.toml"),
        "restore_session = false\nrun_in_background = false\nauto_update = false\nstar_asked = true\nstarts = 1\n",
    )
}

/// Removes the test folders.
pub fn delete_test_files() -> std::io::Result<()> {
    let d = bench_dir();
    if d.exists() { std::fs::remove_dir_all(d) } else { Ok(()) }
}

fn free_space(p: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    unsafe {
        let _ = windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(windows::core::PCWSTR(w.as_ptr()), Some(&mut free), None, None);
    }
    free
}

// ------------------------------------------------------------------ running

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    // an even count averages the middle two (taking v[n / 2] of two runs took the slower one)
    Some(if n % 2 == 0 { (v[n / 2 - 1] + v[n / 2]) / 2.0 } else { v[n / 2] })
}

fn time<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<f64> {
    let t = Instant::now();
    f()?;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}

/// Our engine exactly as a paste uses it (thread count chosen for the disks).
fn our_copy(sources: &[PathBuf], dest: &Path) -> anyhow::Result<()> {
    let tracker = Arc::new(rf_win::fileops::Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false))));
    anyhow::ensure!(rf_win::fastcopy::transfer(rf_win::fileops::Transfer::Copy, sources, dest, tracker)?, "the copy stopped");
    Ok(())
}

fn versus(runs: usize, scratch: &Path, ours: &dyn Fn(&Path) -> anyhow::Result<()>, windows: &dyn Fn(&Path) -> anyhow::Result<()>, cancel: &AtomicBool) -> Versus {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let (mut ac, mut bc, mut am, mut bm) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for r in 0..runs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        for (which, f) in [(0, ours), (1, windows)] {
            let dest = scratch.join(format!("run {r} {which}"));
            let _ = std::fs::remove_dir_all(&dest);
            if std::fs::create_dir_all(&dest).is_err() {
                continue;
            }
            match rf_win::bench::measure_in_process(|| f(&dest)) {
                (Ok(()), cost) => {
                    [&mut a, &mut b][which].push(cost.ms);
                    [&mut ac, &mut bc][which].push(cost.cpu_ms);
                    [&mut am, &mut bm][which].push(cost.extra_mb);
                }
                (Err(e), _) => log::warn!("bench: {} failed: {e:#}", if which == 0 { "ours" } else { "windows" }),
            }
            let _ = std::fs::remove_dir_all(&dest);
        }
        // a side that takes this long is measured once (Windows' ZIP and small-file copy run
        // for half a minute each)
        if a.iter().chain(&b).any(|&ms| ms > 8000.0) {
            break;
        }
    }
    Versus { ours_ms: median(a), windows_ms: median(b), ours_cpu_ms: median(ac), windows_cpu_ms: median(bc), ours_mb: median(am), windows_mb: median(bm), ..Default::default() }
}

/// Runs `plan`. `progress` is called from this thread; `cancel` stops between steps.
// ------------------------------------------------------------------ Files' session

/// Files remembers the tabs open when its last window closed and opens them again on start
/// ("continue where you left off"). Every window the test opens and closes would become the
/// user's "last session" (their next Files start showing an rf-bench folder), and each test start
/// would also restore the previous test's tab. For the test, Files starts without a session;
/// afterwards the user's two settings are written back as they were (also when the test fails
/// or is cancelled: on drop), and again a few seconds later in case Files saved late.
struct FilesSession {
    path: PathBuf,
    saved: Vec<(&'static str, Option<serde_json::Value>)>,
}

const FILES_SESSION_KEYS: [&str; 3] = ["ContinueLastSessionOnStartUp", "LastSessionTabList", "LastSessionSelectedTabIndex"];

impl FilesSession {
    fn settings_path() -> Option<PathBuf> {
        let base = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Packages");
        // the Store package (Files_1y0xx7n9077q4) or a sideloaded one (FilesStable_…, FilesPreview_…)
        std::fs::read_dir(&base)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Files")))
            .map(|p| p.join("LocalState").join("settings").join("user_settings.json"))
            .find(|p| p.is_file())
    }

    fn read(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()? {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Files' own layout: two-space indent, CRLF.
    fn write(path: &Path, m: &serde_json::Map<String, serde_json::Value>) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(m).map_err(std::io::Error::other)?.replace('\n', "\r\n");
        let tmp = path.with_extension("json.rf-bench");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    fn begin() -> Option<Self> {
        let path = Self::settings_path()?;
        let mut m = Self::read(&path)?;
        let saved = FILES_SESSION_KEYS.iter().map(|&k| (k, m.get(k).cloned())).collect();
        m.insert("ContinueLastSessionOnStartUp".into(), serde_json::Value::Bool(false));
        if let Err(e) = Self::write(&path, &m) {
            log::warn!("bench: Files settings not changed: {e}");
            return None;
        }
        log::info!("bench: Files starts without its last session during the test");
        Some(Self { path, saved })
    }

    fn restore(path: &Path, saved: &[(&'static str, Option<serde_json::Value>)]) -> bool {
        let Some(mut m) = Self::read(path) else { return false };
        if saved.iter().all(|(k, v)| m.get(*k) == v.as_ref()) {
            return true;
        }
        for (k, v) in saved {
            match v {
                Some(v) => m.insert((*k).into(), v.clone()),
                None => m.remove(*k),
            };
        }
        Self::write(path, &m).is_ok()
    }
}

impl Drop for FilesSession {
    fn drop(&mut self) {
        if !Self::restore(&self.path, &self.saved) {
            log::warn!("bench: Files settings could not be put back");
        }
        let (path, saved) = (self.path.clone(), std::mem::take(&mut self.saved));
        std::thread::spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_secs(5));
                Self::restore(&path, &saved);
            }
        });
    }
}

pub fn run(plan: &Plan, progress: &dyn Fn(Progress), cancel: &AtomicBool) -> anyhow::Result<Results> {
    let _files_session = if plan.apps.iter().any(|a| a.kind == AppKind::Files) { FilesSession::begin() } else { None };
    let root = bench_dir();
    std::fs::create_dir_all(&root)?;
    prepare_smoothyfiles_settings()?;
    let machine = rf_win::bench::machine(&root);

    // test folders
    for t in &plan.open {
        let dir = root.join(t.folder());
        let label = t.folder().to_string();
        let report = |f: f32| progress(Progress::Preparing(label.clone(), f));
        match t {
            OpenTest::Start => std::fs::create_dir_all(&dir)?,
            OpenTest::Files1k => make_files(&dir, 1_000, &report)?,
            OpenTest::Files10k => make_files(&dir, 10_000, &report)?,
            OpenTest::Files100k => make_files(&dir, 100_000, &report)?,
            OpenTest::Images => make_images(&dir, 2_000, &report)?,
        }
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
    }
    let copy_ok = plan.copy && free_space(&root) > 4 << 30;
    let copy_src = if copy_ok || plan.engines {
        Some(make_copy_sources(&root, &|f| progress(Progress::Preparing("copy".into(), f)))?)
    } else {
        None
    };

    let n_open = plan.open.len() * plan.apps.len() * (RUNS + 1);
    let n = n_open + if copy_ok { 2 } else { 0 } + if plan.engines { 2 } else { 0 };
    let mut i = 0usize;
    let mut step = |label: String| {
        i += 1;
        progress(Progress::Step { i, n, label });
    };

    let mut open = Vec::new();
    for &t in &plan.open {
        let dir = root.join(t.folder());
        let mut per_app: Vec<AppRuns> = plan.apps.iter().map(|a| AppRuns { app: a.kind, runs: Vec::new(), error: None }).collect();
        // run 0 warms up (caches, the app's own first start); apps take turns each round
        for r in 0..=RUNS {
            for (k, app) in plan.apps.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    anyhow::bail!("cancelled");
                }
                step(format!("{} · {}", app.kind.name(), t.folder()));
                if per_app[k].error.is_some() {
                    continue;
                }
                match rf_win::bench::measure_open(app, &dir, OPEN_TIMEOUT, cancel) {
                    Ok(res) if r > 0 => per_app[k].runs.push(res),
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("bench: {} on {}: {e:#}", app.kind.name(), t.folder());
                        per_app[k].error = Some(format!("{e:#}"));
                    }
                }
                // let the closed window's process settle before the next start
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        open.push(OpenResults { test: t, apps: per_app });
    }

    let mut engines = Engines::default();
    let scratch = root.join("scratch");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    if let (true, Some((small, big))) = (copy_ok, &copy_src) {
        step("copy: 10 000 small files".into());
        let s = [small.clone()];
        let mut v = versus(2, &scratch, &|d| our_copy(&s, d), &|d| rf_win::bench::shell_copy_quiet(&s, d), cancel);
        v.files = 10_000;
        v.bytes = 10_000 * 4096;
        engines.copy_small = Some(v);
        step("copy: one 1 GB file".into());
        let b = [big.clone()];
        let mut v = versus(2, &scratch, &|d| our_copy(&b, d), &|d| rf_win::bench::shell_copy_quiet(&b, d), cancel);
        v.files = 1;
        v.bytes = 1 << 30;
        engines.copy_big = Some(v);
    }
    if plan.engines && !cancel.load(Ordering::Relaxed) {
        step("ZIP".into());
        let src = root.join(OpenTest::Files10k.folder());
        let zip = root.join("rf-bench 10000.zip");
        if !zip.exists() && src.exists() {
            let _ = rf_archive::compress(&[src.clone()], &zip, rf_archive::Format::Zip, 5, &mut |_| {}, &|| false);
        }
        if zip.exists() {
            // Windows: copying out of the compressed folder (what Explorer's Extract does)
            let inner = zip.join(OpenTest::Files10k.folder());
            let mut v = versus(
                2,
                &scratch,
                &|d| rf_archive::extract(&zip, d, &mut |_| {}, &|| false).map(|_| ()).map_err(|e| anyhow::anyhow!("{e}")),
                &|d| rf_win::bench::shell_copy_quiet(std::slice::from_ref(&inner), d),
                cancel,
            );
            v.files = 10_000;
            engines.unzip = Some(v);
        }
        step("search".into());
        let q = rf_win::search::SearchQuery { root: root.clone(), pattern: "file 0999".into(), include_hidden: false, max_results: 50_000 };
        if let (Ok(s), cost) = rf_win::bench::measure_in_process(|| rf_win::search::search(&q, &|| false, &mut |_| {})) {
            engines.search_ms = Some(cost.ms);
            engines.search_cpu_ms = Some(cost.cpu_ms);
            engines.search_mb = Some(cost.extra_mb);
            engines.search_entries = s.entries_scanned as u64;
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    let when = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let r = Results { when, version: crate::installer::version(), machine, open, engines };
    let _ = std::fs::write(root.join("results.json"), serde_json::to_vec_pretty(&r).unwrap_or_default());
    Ok(r)
}

/// The last results, if a test was run before.
pub fn last_results() -> Option<Results> {
    serde_json::from_slice(&std::fs::read(bench_dir().join("results.json")).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine comparison reports what each side cost: a real copy of 300 small files with
    /// our engine and with Windows' (in this process), both measured.
    #[test]
    fn engine_comparison_reports_cpu_and_memory() {
        let root = std::env::temp_dir().join(format!("rf-bench-cost-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..300 {
            std::fs::write(src.join(format!("f{i}.bin")), vec![7u8; 8192]).unwrap();
        }
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let s = [src.clone()];
        let v = versus(1, &scratch, &|d| our_copy(&s, d), &|d| rf_win::bench::shell_copy_quiet(&s, d), &AtomicBool::new(false));
        assert!(v.ours_ms.is_some() && v.windows_ms.is_some(), "{v:?}");
        assert!(v.ours_cpu_ms.is_some_and(|c| c >= 0.0) && v.windows_cpu_ms.is_some_and(|c| c > 0.0), "{v:?}");
        assert!(v.ours_mb.is_some_and(|m| m >= 0.0) && v.windows_mb.is_some_and(|m| m >= 0.0), "{v:?}");
        eprintln!("{v:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Files' session settings are put back exactly, other settings Files changed meanwhile kept.
    #[test]
    fn files_session_is_put_back() {
        let dir = std::env::temp_dir().join(format!("rf-files-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user_settings.json");
        std::fs::write(&path, "{\r\n  \"ShowShelfPane\": false,\r\n  \"ContinueLastSessionOnStartUp\": true,\r\n  \"LastSessionTabList\": [\r\n    \"C:\\\\Users\"\r\n  ]\r\n}").unwrap();
        let mut m = FilesSession::read(&path).unwrap();
        let saved: Vec<_> = FILES_SESSION_KEYS.iter().map(|&k| (k, m.get(k).cloned())).collect();
        // what the test leaves: Files' own writes of the bench tabs, plus an unrelated change
        m.insert("ContinueLastSessionOnStartUp".into(), false.into());
        m.insert("LastSessionTabList".into(), serde_json::json!(["C:\\rf-bench 1000"]));
        m.insert("LastSessionSelectedTabIndex".into(), 0.into());
        m.insert("ShowShelfPane".into(), true.into());
        FilesSession::write(&path, &m).unwrap();
        assert!(FilesSession::restore(&path, &saved));
        let back = FilesSession::read(&path).unwrap();
        assert_eq!(back["ContinueLastSessionOnStartUp"], true);
        assert_eq!(back["LastSessionTabList"], serde_json::json!(["C:\\Users"]));
        assert!(!back.contains_key("LastSessionSelectedTabIndex"));
        assert_eq!(back["ShowShelfPane"], true);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\r\n  \"ShowShelfPane\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Results saved by 0.5.0 (no cost fields) still load.
    #[test]
    fn older_results_still_load() {
        let old = r#"{"ours_ms":1.0,"windows_ms":2.0,"bytes":0,"files":0}"#;
        let v: Versus = serde_json::from_str(old).unwrap();
        assert_eq!(v.ours_cpu_ms, None);
    }
}

#[cfg(test)]
mod files_live {
    /// Opens the 1 000-file folder in Files (when installed) through the real test and checks that
    /// Files' session settings are the user's again afterwards. Opens windows: run by hand
    /// (`cargo test -p rf-app --lib files_live -- --ignored`).
    #[test]
    #[ignore]
    fn files_session_survives_a_real_test() {
        let Some(files) = rf_win::bench::detect().into_iter().find(|a| a.kind == rf_win::bench::AppKind::Files) else { return };
        let path = super::FilesSession::settings_path().unwrap();
        let before = super::FilesSession::read(&path).unwrap();
        let plan = super::Plan { apps: vec![files], open: vec![super::OpenTest::Files1k], copy: false, engines: false };
        let r = super::run(&plan, &|_| {}, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        eprintln!("{:?}", r.open[0].apps[0].median().map(|m| (m.settled_ms, m.shared)));
        std::thread::sleep(std::time::Duration::from_secs(17));
        let after = super::FilesSession::read(&path).unwrap();
        for k in super::FILES_SESSION_KEYS {
            assert_eq!(before.get(k), after.get(k), "{k}");
        }
    }
}

#[cfg(test)]
mod live_100k {
    /// The 100 000-file folder: smoothyfiles from scratch, smoothyfiles already running (a copy with
    /// the test's own settings, kept running in the background: its first start is the warm-up
    /// run, then each start hands the folder to it, as README measures), Explorer and Files.
    /// Never touches the user's copy: skipped while any other smoothyfiles runs. Opens windows: run
    /// by hand (`cargo test -p rf-app --lib live_100k -- --ignored --nocapture`), then end the
    /// test's copy (it stays in the background).
    #[test]
    #[ignore]
    fn open_100k() {
        use rf_win::bench::{App, AppKind};
        let fresh = super::smoothyfiles_app();
        let others = std::process::Command::new("tasklist").args(["/FI", "IMAGENAME eq smoothyfiles.exe", "/NH"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("smoothyfiles.exe")).unwrap_or(true);
        assert!(!others, "a smoothyfiles is running: the warm runs would go to it");
        let cfg = super::bench_dir().join("smoothyfiles-warm");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("settings.toml"), "restore_session = false
run_in_background = true
auto_update = false
star_asked = true
starts = 1
search_index = false
name_index = false
directlook_enabled = false
").unwrap();
        let warm = App {
            kind: AppKind::Smoothyfiles,
            exe: fresh.exe.clone(),
            env: vec![("SMOOTHYFILES_CONFIG_DIR".into(), cfg.to_string_lossy().into_owned()), ("SMOOTHYFILES_MUTE".into(), "1".into())],
        };
        let mut apps = vec![fresh, warm];
        apps.extend(rf_win::bench::detect().into_iter().filter(|a| matches!(a.kind, AppKind::Explorer | AppKind::Files)));
        let names = ["smoothyfiles, from scratch", "smoothyfiles, already running", "File Explorer", "Files"];
        let plan = super::Plan { apps, open: vec![super::OpenTest::Files100k], copy: false, engines: false };
        let r = super::run(&plan, &|_| {}, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        for (name, a) in names.iter().zip(&r.open[0].apps) {
            let runs: Vec<String> = a.runs.iter().map(|x| format!("{:.0}{}{}", x.settled_ms, if x.shared { "*" } else { "" }, if x.timed_out { "!" } else { "" })).collect();
            eprintln!("{name}: median {:?} ms; window {:?} ms; runs [{}] {:?}", a.median().map(|m| m.settled_ms.round()), a.median().map(|m| m.window_ms.round()), runs.join(", "), a.error);
        }
    }
}
