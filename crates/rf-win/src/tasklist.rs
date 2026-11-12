//! The windows a taskbar shows, found with the same rules as Explorer's taskbar, grouped by
//! application (first step of our own taskbar, see notes/design: görev çubuğu). Read-only:
//! nothing here changes any window.
//!
//! A window is a task when it is visible, not cloaked by DWM (other virtual desktops, suspended
//! UWP frames), and either has no owner and isn't a tool window, or asks for a taskbar button
//! with `WS_EX_APPWINDOW`. Windows are grouped by their AppUserModelID, or by their program
//! when they have none — as Explorer groups them.

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

/// One taskbar button's window.
#[derive(Clone, Debug)]
pub struct Task {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    /// The program, when it can be read (not for elevated processes of other users).
    pub exe: Option<PathBuf>,
    /// The window's AppUserModelID, when it sets one.
    pub app_id: Option<String>,
    pub minimized: bool,
}

impl Task {
    /// What Explorer groups by: the app id, else the program path (case-insensitive).
    pub fn group(&self) -> String {
        match (&self.app_id, &self.exe) {
            (Some(id), _) => id.to_lowercase(),
            (None, Some(exe)) => exe.to_string_lossy().to_lowercase(),
            (None, None) => format!("pid:{}", self.pid),
        }
    }
}

/// The style rules alone (testable without windows): owner, extended style.
pub fn style_is_task(has_owner: bool, ex_style: u32) -> bool {
    let app = ex_style & WS_EX_APPWINDOW.0 != 0;
    if app {
        return true;
    }
    !has_owner && ex_style & WS_EX_TOOLWINDOW.0 == 0 && ex_style & WS_EX_NOACTIVATE.0 == 0
}

fn cloaked(h: HWND) -> bool {
    let mut v: u32 = 0;
    unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut v as *mut u32 as *mut _, 4) }.is_ok() && v != 0
}

fn title(h: HWND) -> String {
    let n = unsafe { GetWindowTextLengthW(h) };
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

pub(crate) fn exe_of(pid: u32) -> Option<PathBuf> {
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(p);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// The window's AppUserModelID (`System.AppUserModel.ID` in its property store).
fn app_id_of(h: HWND) -> Option<String> {
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(h).ok()?;
        let v = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let p = PropVariantToStringAlloc(&v).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s.filter(|s| !s.is_empty())
    }
}

fn task_of(h: HWND) -> Option<Task> {
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return None;
        }
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let has_owner = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
        if !style_is_task(has_owner, ex) || cloaked(h) {
            return None;
        }
        let mut r = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let title = title(h);
        // untitled, zero-size helpers are not buttons (Explorer skips them too)
        if title.is_empty() && (r.right - r.left <= 0 || r.bottom - r.top <= 0) {
            return None;
        }
        Some(Task { hwnd: h.0 as isize, title, pid, exe: exe_of(pid), app_id: app_id_of(h), minimized: IsIconic(h).as_bool() })
    }
}

/// The current task windows, in Z order (the active one first).
pub fn tasks() -> Vec<Task> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let out = unsafe { &mut *(l.0 as *mut Vec<Task>) };
        if let Some(t) = task_of(h) {
            out.push(t);
        }
        BOOL(1)
    }
    let mut out: Vec<Task> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut Vec<Task> as isize));
    }
    out
}

/// Tasks grouped like taskbar buttons: (group key, its windows), in order of first appearance.
pub fn grouped(tasks: Vec<Task>) -> Vec<(String, Vec<Task>)> {
    let mut groups: Vec<(String, Vec<Task>)> = Vec::new();
    for t in tasks {
        let k = t.group();
        match groups.iter_mut().find(|g| g.0 == k) {
            Some(g) => g.1.push(t),
            None => groups.push((k, vec![t])),
        }
    }
    groups
}

// ---------- change tracking ----------

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// What changed among the task windows (coalesced over a frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Windows appeared, went away, or were shown / hidden / cloaked: the list changed.
    pub list: bool,
    /// A title changed.
    pub titles: bool,
    /// Another window came to the front, or one was minimized / restored.
    pub active: bool,
}

thread_local! {
    static EVENTS: RefCell<Option<Sender<u32>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn on_event(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // only whole windows, not their parts (carets, scroll bars, list items)
    if id_object != 0 || id_child != 0 || hwnd.is_invalid() {
        return;
    }
    EVENTS.with(|e| {
        if let Some(tx) = e.borrow().as_ref() {
            let _ = tx.send(event);
        }
    });
}

/// Watches the task windows; `on_change` runs on a background thread, at most once a frame
/// (~16 ms) with what changed. Drop to stop.
pub struct TaskWatch {
    thread: u32,
}

impl Drop for TaskWatch {
    fn drop(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_QUIT, windows::Win32::Foundation::WPARAM(0), LPARAM(0));
        }
    }
}

fn classify(event: u32, c: &mut Changes) {
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
        EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    };
    match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_DESTROY | EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED | EVENT_OBJECT_UNCLOAKED => c.list = true,
        EVENT_OBJECT_NAMECHANGE => c.titles = true,
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND => c.active = true,
        _ => {}
    }
}

/// Starts watching. The hooks live on their own thread with a message loop (out-of-context
/// WinEvents are delivered there); a second thread coalesces them.
pub fn watch(on_change: impl Fn(Changes) + Send + 'static) -> anyhow::Result<TaskWatch> {
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GetMessageW, MSG, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    };
    let (tx, rx): (Sender<u32>, Receiver<u32>) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u32>();
    std::thread::Builder::new().name("rf-taskwatch".into()).spawn(move || {
        EVENTS.with(|e| *e.borrow_mut() = Some(tx));
        let ranges = [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ];
        let hooks: Vec<_> = ranges.iter().map(|&(a, b)| unsafe { SetWinEventHook(a, b, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT) }).collect();
        let _ = ready_tx.send(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            for h in hooks {
                let _ = UnhookWinEvent(h);
            }
        }
        EVENTS.with(|e| *e.borrow_mut() = None);
    })?;
    let thread = ready_rx.recv_timeout(Duration::from_secs(5))?;
    std::thread::Builder::new().name("rf-taskwatch-coalesce".into()).spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut c = Changes::default();
            classify(first, &mut c);
            let until = std::time::Instant::now() + Duration::from_millis(16);
            loop {
                let left = until.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(left) {
                    Ok(e) => classify(e, &mut c),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(_) => return,
                }
            }
            if c != Changes::default() {
                on_change(c);
            }
        }
    })?;
    Ok(TaskWatch { thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_rules_match_explorer() {
        let tool = WS_EX_TOOLWINDOW.0;
        let app = WS_EX_APPWINDOW.0;
        assert!(style_is_task(false, 0), "a plain top-level window");
        assert!(!style_is_task(true, 0), "an owned dialog has no button");
        assert!(!style_is_task(false, tool), "tool windows have none");
        assert!(style_is_task(true, app), "APPWINDOW forces a button, even owned");
        assert!(style_is_task(false, tool | app), "APPWINDOW wins over TOOLWINDOW");
        assert!(!style_is_task(false, WS_EX_NOACTIVATE.0));
    }

    #[test]
    fn groups_by_app_id_then_program() {
        let t = |pid, exe: &str, id: Option<&str>| Task { hwnd: pid as isize, title: String::new(), pid, exe: Some(PathBuf::from(exe)), app_id: id.map(String::from), minimized: false };
        let g = grouped(vec![
            t(1, r"C:\a\Chrome.exe", None),
            t(2, r"C:\b\code.exe", Some("Microsoft.VisualStudioCode")),
            t(3, r"C:\A\chrome.exe", None),
            t(4, r"C:\c\other.exe", Some("microsoft.visualstudiocode")),
        ]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].1.iter().map(|t| t.pid).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(g[1].1.iter().map(|t| t.pid).collect::<Vec<_>>(), vec![2, 4]);
    }

    #[test]
    fn notices_a_new_window_quickly() {
        use std::sync::{Arc, Mutex};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SW_SHOWNOACTIVATE, ShowWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
        };
        let seen: Arc<Mutex<Vec<(std::time::Instant, Changes)>>> = Default::default();
        let s2 = seen.clone();
        let _w = watch(move |c| s2.lock().unwrap().push((std::time::Instant::now(), c))).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        seen.lock().unwrap().clear();
        // an off-screen, never-activated window, from this thread
        let class = crate::wide::wide("STATIC");
        let title = crate::wide::wide("rf tasklist test");
        let shown_at;
        let h = unsafe {
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                -4000,
                -4000,
                200,
                100,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            shown_at = std::time::Instant::now();
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
            h
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut latency = None;
        while std::time::Instant::now() < deadline {
            if let Some((t, _)) = seen.lock().unwrap().iter().find(|(_, c)| c.list) {
                latency = Some(t.duration_since(shown_at));
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let listed = tasks().iter().any(|t| t.hwnd == h.0 as isize && t.title == "rf tasklist test");
        unsafe {
            let _ = DestroyWindow(h);
        }
        let latency = latency.expect("the new window was reported");
        assert!(listed, "and it is a task");
        assert!(latency < Duration::from_millis(500), "reported after {latency:?}");
        eprintln!("new window reported after {latency:?}");
    }

    #[test]
    fn lists_without_failing() {
        // whatever is open on the test machine: each listed task has a pid and is visible
        for t in tasks() {
            assert!(t.pid != 0);
        }
    }
}
