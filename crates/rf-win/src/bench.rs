//! Benchmark measurements of file managers on the user's machine (Settings → Speed test).
//!
//! An app is started with a folder; its new window is found by the folder's name in the title,
//! sized to 1200 × 800, and captured with `PrintWindow` every ~30 ms. The folder counts as
//! open when the picture last changed and then stayed the same for [`QUIET`]: the time of that
//! last change is the result (the wait itself is not counted). The same method produced the
//! numbers on latief.id/smoothyfiles (notes/benchmarks/2026-11-06).
//!
//! Alongside: the CPU time and memory of the process that owns the window. When that process
//! was already running (Explorer's shell, a Files window the user had open) the numbers are
//! marked shared: CPU is counted from the moment the window appeared, memory is the whole
//! process.
//!
//! Only windows this run opened are touched, and only with `WM_CLOSE`; other apps' processes
//! are never killed.

use std::collections::HashSet;

use image::ImageEncoder as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, MoveWindow, PostMessageW, WM_CLOSE,
};
use windows::core::BOOL;

/// The picture must stay the same this long for the folder to count as open.
pub const QUIET: Duration = Duration::from_millis(500);
/// Window size every app is measured at (physical pixels).
pub const SIZE: (i32, i32) = (1200, 800);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AppKind {
    Smoothyfiles,
    Explorer,
    Files,
    FilePilot,
}

impl AppKind {
    pub fn name(self) -> &'static str {
        match self {
            AppKind::Smoothyfiles => "smoothyfiles",
            AppKind::Explorer => "File Explorer",
            AppKind::Files => "Files",
            AppKind::FilePilot => "File Pilot",
        }
    }
}

/// An app that can be measured: how to start it with a folder.
#[derive(Debug, Clone)]
pub struct App {
    pub kind: AppKind,
    pub exe: PathBuf,
    pub env: Vec<(String, String)>,
}

/// The other file managers installed here (Explorer always is).
pub fn detect() -> Vec<App> {
    let mut out = Vec::new();
    let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    out.push(App { kind: AppKind::Explorer, exe: windir.join("explorer.exe"), env: Vec::new() });
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        // Files (Store / sideloaded): its app execution alias
        let aliases = local.join(r"Microsoft\WindowsApps");
        if let Some(exe) = ["files.exe", "files-stable.exe", "files-preview.exe"].iter().map(|n| aliases.join(n)).find(|p| p.exists()) {
            out.push(App { kind: AppKind::Files, exe, env: Vec::new() });
        }
    }
    if let Some(exe) = file_pilot() {
        out.push(App { kind: AppKind::FilePilot, exe, env: Vec::new() });
    }
    out
}

/// File Pilot's executable, from its uninstall entry (per user or per machine).
fn file_pilot() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW};
    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let w = crate::wide::wide(UNINSTALL);
        let mut key = HKEY::default();
        if unsafe { RegOpenKeyExW(root, windows::core::PCWSTR(w.as_ptr()), Some(0), KEY_READ, &mut key) }.is_err() {
            continue;
        }
        let mut found = None;
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if unsafe { RegEnumKeyExW(key, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
                break;
            }
            let sub = format!(r"{UNINSTALL}\{}", String::from_utf16_lossy(&name[..len as usize]));
            let display = reg_string(root, &sub, "DisplayName").unwrap_or_default();
            if !display.to_lowercase().contains("file pilot") && !display.to_lowercase().contains("filepilot") {
                continue;
            }
            let icon = reg_string(root, &sub, "DisplayIcon").unwrap_or_default();
            let exe = PathBuf::from(icon.split(',').next().unwrap_or("").trim_matches('"'));
            if exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) && exe.exists() {
                found = Some(exe);
                break;
            }
            let dir = reg_string(root, &sub, "InstallLocation").unwrap_or_default();
            if let Ok(rd) = std::fs::read_dir(dir.trim_matches('"')) {
                if let Some(e) = rd.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) && !p.to_string_lossy().to_lowercase().contains("unins")) {
                    found = Some(e);
                    break;
                }
            }
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

/// A string value from the registry.
fn reg_string(root: windows::Win32::System::Registry::HKEY, key: &str, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::{RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};
    let (k, v) = (crate::wide::wide(key), crate::wide::wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            root,
            windows::core::PCWSTR(k.as_ptr()),
            windows::core::PCWSTR(v.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
    }
    .ok()
    .ok()?;
    let n = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    Some(s.trim_end_matches('\0').to_string())
}

// ------------------------------------------------------------------ windows

#[derive(Debug, Clone)]
struct Win {
    hwnd: isize,
    pid: u32,
    title: String,
}

fn windows() -> Vec<Win> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let out = unsafe { &mut *(l.0 as *mut Vec<Win>) };
        unsafe {
            if IsWindowVisible(h).as_bool() {
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                let mut buf = [0u16; 512];
                let n = GetWindowTextW(h, &mut buf);
                out.push(Win { hwnd: h.0 as isize, pid, title: String::from_utf16_lossy(&buf[..n.max(0) as usize]) });
            }
        }
        BOOL(1)
    }
    let mut out: Vec<Win> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut _)
}

/// A hash of the window's picture (`None` when it can't be captured).
struct Grabber {
    dc: windows::Win32::Graphics::Gdi::HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    bits: *mut u8,
    len: usize,
}

impl Grabber {
    fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            SelectObject(dc, bmp.into());
            Some(Self { dc, bmp, bits: bits as *mut u8, len: (w * h * 4) as usize })
        }
    }

    fn hash(&mut self, h: HWND) -> Option<u64> {
        unsafe {
            std::ptr::write_bytes(self.bits, 0, self.len);
            // PW_RENDERFULLCONTENT: DirectComposition / WinUI content too
            if !PrintWindow(h, self.dc, PRINT_WINDOW_FLAGS(2)).as_bool() {
                return None;
            }
            let px = std::slice::from_raw_parts(self.bits as *const u64, self.len / 8);
            // FNV-style over every other 8-byte word: fast, and any visible change moves it
            let mut x = 0xcbf2_9ce4_8422_2325u64;
            for v in px.iter().step_by(2) {
                x = (x ^ v).wrapping_mul(0x0100_0000_01b3);
            }
            Some(x)
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ------------------------------------------------------------------ processes

struct Proc(HANDLE);

impl Proc {
    fn open(pid: u32) -> Option<Self> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | windows::Win32::System::Threading::PROCESS_TERMINATE, false, pid) }
            .or_else(|_| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
            .ok()
            .map(Proc)
    }

    /// (creation time as FILETIME ticks, CPU time kernel + user in ms)
    fn times(&self) -> Option<(u64, f64)> {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(self.0, &mut c, &mut e, &mut k, &mut u) }.ok()?;
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        Some((t(c), (t(k) + t(u)) as f64 / 10_000.0))
    }

    /// (working set, peak working set, private bytes, private working set) in bytes. The last
    /// is Task Manager's "Memory" column.
    fn memory(&self) -> Option<(u64, u64, u64, u64)> {
        let mut m = PROCESS_MEMORY_COUNTERS_EX2 { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
        unsafe { GetProcessMemoryInfo(self.0, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) }.ok()?;
        Some((m.WorkingSetSize as u64, m.PeakWorkingSetSize as u64, m.PrivateUsage as u64, m.PrivateWorkingSetSize as u64))
    }

    fn image(&self) -> String {
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        unsafe { QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut n) }
            .map(|_| String::from_utf16_lossy(&buf[..n as usize]))
            .unwrap_or_default()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn now_filetime() -> u64 {
    let f = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

// ------------------------------------------------------------------ measuring

/// One app opening one folder.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OpenResult {
    /// Start → the window's picture stopped changing (ms).
    pub settled_ms: f64,
    /// Start → its window appeared (ms).
    pub window_ms: f64,
    /// CPU time of the owning process (ms; from the window's appearance when `shared`).
    pub cpu_ms: f64,
    /// Working set when settled, and the process's peak (bytes; peak is 0 when `shared`).
    pub ram: u64,
    pub peak_ram: u64,
    /// Private bytes when settled: memory the process itself allocated (closest to Task
    /// Manager's figure; leaves out mapped files and shared driver memory).
    #[serde(default)]
    pub private: u64,
    /// Private working set when settled: Task Manager's "Memory" column (0 in results saved
    /// before it was measured).
    #[serde(default)]
    pub private_ws: u64,
    /// The window lives in a process that was already running.
    pub shared: bool,
    /// Gave up before the picture settled (`settled_ms` is the last change seen).
    pub timed_out: bool,
    /// When the picture changed (ms from start): the race replay.
    pub changes: Vec<f64>,
    /// The owning process's executable.
    pub process: String,
}

/// Starts `app` on `folder` and measures it (see the module notes). Blocking; `cancel` stops
/// it. The window is closed afterwards.
pub fn measure_open(app: &App, folder: &Path, timeout: Duration, cancel: &AtomicBool) -> anyhow::Result<OpenResult> {
    let leaf = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    anyhow::ensure!(!leaf.is_empty(), "folder without a name");
    measure_launch(app, folder.as_os_str(), &leaf, QUIET, timeout, cancel)
}

/// [`measure_open`] with any argument (a search link, …): the new window is the one whose title
/// contains `title`, and it counts as done after `quiet` without a change.
pub fn measure_launch(app: &App, arg: &std::ffi::OsStr, title: &str, quiet: Duration, timeout: Duration, cancel: &AtomicBool) -> anyhow::Result<OpenResult> {
    let leaf = title.to_string();
    let before: HashSet<isize> = windows().into_iter().map(|w| w.hwnd).collect();
    let launched_at = now_filetime();
    let t0 = Instant::now();
    let mut cmd = Command::new(&app.exe);
    // a search link carries '&': quoted as one argument exactly as written (unquoted, Explorer
    // split it and asked Windows for a "location:" app)
    let text = arg.to_string_lossy();
    if text.contains('&') {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(format!("\"{text}\""));
    } else {
        cmd.arg(arg);
    }
    for (k, v) in &app.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{}: {e}", app.exe.display()))?;
    let child_pid = child.id();

    // the new window titled with the folder's name
    let win = loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        if t0.elapsed() > timeout {
            anyhow::bail!("{}: no window within {:?}", app.kind.name(), timeout);
        }
        let found = windows().into_iter().find(|w| {
            // smoothyfiles started as its own instance must show its own window, never one of the
            // user's copy; started as a single instance, it hands the folder to the running one
            // (the test's own, with the test's settings)
            !before.contains(&w.hwnd)
                && w.title.contains(&leaf)
                && (app.kind != AppKind::Smoothyfiles || !app.env.iter().any(|(k, _)| k == "SMOOTHYFILES_MULTI_INSTANCE") || w.pid == child_pid)
        });
        if let Some(w) = found {
            break w;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let window_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let h = hwnd(win.hwnd);
    unsafe {
        let _ = MoveWindow(h, 80, 80, SIZE.0, SIZE.1, true);
        // On top of everything, without taking the focus: Windows opens a window started from
        // the background behind the active one, and some apps draw less (or not at all) while
        // covered, which would skew the comparison. Every app is measured visible.
        use windows::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos};
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
    }
    let proc = Proc::open(win.pid);
    let shared = proc.as_ref().and_then(|p| p.times()).is_some_and(|(created, _)| created < launched_at);
    let cpu_at_window = proc.as_ref().and_then(|p| p.times()).map_or(0.0, |t| t.1);

    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut rect);
    }
    let (w, hgt) = ((rect.right - rect.left).max(SIZE.0), (rect.bottom - rect.top).max(SIZE.1));
    let mut grab = Grabber::new(w, hgt).ok_or_else(|| anyhow::anyhow!("capture buffer"))?;
    let mut last_hash = None;
    let mut last_change = t0.elapsed();
    let mut changes = Vec::new();
    let mut timed_out = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            close(win.hwnd, &mut child, app.kind);
            anyhow::bail!("cancelled");
        }
        if !unsafe { IsWindow(Some(h)) }.as_bool() {
            anyhow::bail!("{}: the window closed by itself", app.kind.name());
        }
        let now = t0.elapsed();
        if let Some(x) = grab.hash(h) {
            if Some(x) != last_hash {
                last_hash = Some(x);
                last_change = now;
                changes.push(now.as_secs_f64() * 1000.0);
            }
        }
        if now.saturating_sub(last_change) >= quiet && last_hash.is_some() {
            break;
        }
        if now > timeout {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let (cpu_ms, ram, peak_ram, private, private_ws, process) = match &proc {
        Some(p) => {
            let cpu = p.times().map_or(0.0, |t| t.1);
            let (ws, peak, private, private_ws) = p.memory().unwrap_or((0, 0, 0, 0));
            (if shared { cpu - cpu_at_window } else { cpu }, ws, if shared { 0 } else { peak }, private, private_ws, p.image())
        }
        None => (0.0, 0, 0, 0, 0, String::new()),
    };
    close(win.hwnd, &mut child, app.kind);
    Ok(OpenResult {
        settled_ms: last_change.as_secs_f64() * 1000.0,
        window_ms,
        cpu_ms,
        ram,
        peak_ram,
        private,
        private_ws,
        shared,
        timed_out,
        changes,
        process,
    })
}

/// Closes the window this run opened; waits up to 5 s. Our own child (smoothyfiles) is ended if
/// it is still there; other apps' processes never are.
fn close(win: isize, child: &mut std::process::Child, kind: AppKind) {
    let h = hwnd(win);
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    let t = Instant::now();
    while unsafe { IsWindow(Some(h)) }.as_bool() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(30));
    }
    if kind == AppKind::Smoothyfiles {
        if t.elapsed() >= Duration::from_secs(5) || matches!(child.try_wait(), Ok(None)) {
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(30));
            }
            if matches!(child.try_wait(), Ok(None)) {
                if let Some(p) = Proc::open(child.id()) {
                    unsafe {
                        let _ = TerminateProcess(p.0, 0);
                    }
                }
            }
        }
    }
    let _ = child.try_wait();
}

// ------------------------------------------------------------------ the machine

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Machine {
    pub cpu: String,
    pub cores: u32,
    pub ram_gb: u32,
    pub windows: String,
    /// The disk holding the test folders: "SSD", "HDD" or "".
    pub disk: String,
}

pub fn machine(test_dir: &Path) -> Machine {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let cpu = reg_string(HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString")
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let ram_gb = if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { ((m.ullTotalPhys as f64) / (1u64 << 30) as f64).round() as u32 } else { 0 };
    let product = reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "ProductName").unwrap_or_default();
    let build = reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuild").unwrap_or_default();
    // ProductName still says "Windows 10" on Windows 11
    let product = if build.parse::<u32>().is_ok_and(|b| b >= 22000) { product.replace("Windows 10", "Windows 11") } else { product };
    let disk = match crate::drives::seek_penalty(test_dir) {
        Some(true) => "HDD",
        Some(false) => "SSD",
        None => "",
    };
    Machine {
        cpu,
        cores: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
        ram_gb,
        windows: format!("{product} ({build})"),
        disk: disk.into(),
    }
}

// ------------------------------------------------------------------ copying with Windows

/// Copies with the Windows shell's engine (what Explorer uses), without progress UI, prompts or
/// an undo record: the comparison for our copy engine.
pub fn shell_copy_quiet(sources: &[PathBuf], dest_dir: &Path) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::{FOF_NO_UI, FOF_NOCONFIRMMKDIR};
    crate::fileops::shell_copy(sources, dest_dir, FOF_NO_UI.0 as u32 | FOF_NOCONFIRMMKDIR.0 as u32)
}

#[cfg(test)]
mod tests {
    /// Task Manager's figure is read, and it is the smaller, resident part of what the process
    /// allocated.
    #[test]
    fn task_manager_memory_is_read() {
        let p = super::Proc::open(std::process::id()).unwrap();
        let (ws, _, private, private_ws) = p.memory().unwrap();
        assert!(private_ws > 0 && private_ws <= ws && private > 0, "{ws} {private} {private_ws}");
    }

    #[test]
    fn explorer_is_always_found() {
        let apps = super::detect();
        assert_eq!(apps[0].kind, super::AppKind::Explorer);
        assert!(apps[0].exe.exists());
    }

    #[test]
    fn machine_info() {
        let m = super::machine(&std::env::temp_dir());
        assert!(m.cores > 0 && m.ram_gb > 0, "{m:?}");
        assert!(m.windows.starts_with("Windows"), "{m:?}");
    }
}

// ------------------------------------------------------------------ result card picture

/// A picture to share: the card's SVG drawn at `scale` with the system fonts.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Straight (not premultiplied) RGBA, top row first.
    pub rgba: Vec<u8>,
    pub png: Vec<u8>,
}

pub fn render_card(svg: &str, scale: f32) -> anyhow::Result<Picture> {
    use resvg::{tiny_skia, usvg};
    static FONTS: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    let opt = usvg::Options {
        fontdb: FONTS
            .get_or_init(|| {
                let mut db = usvg::fontdb::Database::new();
                db.load_system_fonts();
                std::sync::Arc::new(db)
            })
            .clone(),
        font_family: "Segoe UI".into(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(svg.as_bytes(), &opt)?;
    let size = tree.size();
    let (w, h) = ((size.width() * scale).ceil() as u32, (size.height() * scale).ceil() as u32);
    let mut pix = tiny_skia::Pixmap::new(w.max(1), h.max(1)).ok_or_else(|| anyhow::anyhow!("card size"))?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pix.as_mut());
    let mut rgba = pix.take();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)?;
    Ok(Picture { width: w, height: h, rgba, png })
}

/// Time from `trigger` until the picture of the already open window `hwnd` changed and then
/// stayed the same for `quiet` (ms, with the times of every change): a search sent to a running
/// app, a refresh.
pub fn measure_change(hwnd_raw: isize, trigger: impl FnOnce() -> anyhow::Result<()>, quiet: Duration, timeout: Duration) -> anyhow::Result<(f64, Vec<f64>)> {
    let h = hwnd(hwnd_raw);
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(h, &mut rect);
    }
    let mut grab = Grabber::new((rect.right - rect.left).max(1), (rect.bottom - rect.top).max(1)).ok_or_else(|| anyhow::anyhow!("capture buffer"))?;
    let start_hash = grab.hash(h);
    let t0 = Instant::now();
    trigger()?;
    let mut last_hash = start_hash;
    let mut last_change: Option<Duration> = None;
    let mut changes = Vec::new();
    loop {
        let now = t0.elapsed();
        if let Some(x) = grab.hash(h) {
            if Some(x) != last_hash {
                last_hash = Some(x);
                last_change = Some(now);
                changes.push(now.as_secs_f64() * 1000.0);
            }
        }
        if let Some(lc) = last_change {
            if now.saturating_sub(lc) >= quiet {
                return Ok((lc.as_secs_f64() * 1000.0, changes));
            }
        }
        if now > timeout {
            anyhow::bail!("no settled picture within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The visible top-level windows of a process (handle, title).
pub fn windows_of(pid: u32) -> Vec<(isize, String)> {
    windows().into_iter().filter(|w| w.pid == pid).map(|w| (w.hwnd, w.title)).collect()
}

/// Puts a window on top at the measuring size without taking the focus (as [`measure_launch`]
/// does for the windows it opens).
pub fn place_on_top(hwnd_raw: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos};
    unsafe {
        let _ = SetWindowPos(hwnd(hwnd_raw), Some(HWND_TOPMOST), 80, 80, SIZE.0, SIZE.1, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

/// (working set, private bytes, open handles) of a running process.
pub fn process_memory(pid: u32) -> Option<(u64, u64, u32)> {
    let p = Proc::open(pid)?;
    let (ws, _, private, _) = p.memory()?;
    let mut handles = 0u32;
    unsafe {
        let _ = windows::Win32::System::Threading::GetProcessHandleCount(p.0, &mut handles);
    }
    Some((ws, private, handles))
}

/// Asks a window to close, as Alt+F4 does.
pub fn post_close(hwnd_raw: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
    unsafe {
        let _ = PostMessageW(Some(hwnd(hwnd_raw)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// What some work run inside this process cost: wall time, CPU time of the whole process
/// (every thread: an engine's workers, the shell's copy engine), and the private memory it
/// took on top of what the process held before (sampled every 5 ms, the peak).
#[derive(Debug, Clone, Copy)]
pub struct InProcess {
    pub ms: f64,
    pub cpu_ms: f64,
    pub extra_mb: f64,
}

/// Runs `f` and reports [`InProcess`] for it.
pub fn measure_in_process<T>(f: impl FnOnce() -> T) -> (T, InProcess) {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    let me = Proc::open(std::process::id());
    let private = |p: &Option<Proc>| p.as_ref().and_then(|p| p.memory()).map_or(0, |m| m.2);
    let start_mem = private(&me);
    let start_cpu = me.as_ref().and_then(|p| p.times()).map_or(0.0, |t| t.1);
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(start_mem));
    let sampler = {
        let (done, peak) = (done.clone(), peak.clone());
        std::thread::spawn(move || {
            let me = Proc::open(std::process::id());
            while !done.load(Ordering::Relaxed) {
                let m = me.as_ref().and_then(|p| p.memory()).map_or(0, |m| m.2);
                peak.fetch_max(m, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let t = Instant::now();
    let out = f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    done.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    let end_cpu = me.as_ref().and_then(|p| p.times()).map_or(0.0, |t| t.1);
    peak.fetch_max(private(&me), Ordering::Relaxed);
    let extra = peak.load(Ordering::Relaxed).saturating_sub(start_mem);
    (out, InProcess { ms, cpu_ms: (end_cpu - start_cpu).max(0.0), extra_mb: extra as f64 / 1048576.0 })
}
