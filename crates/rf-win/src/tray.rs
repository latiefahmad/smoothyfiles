//! Notification-area (tray) icon and notifications, like Files' "keep running in the
//! background": the icon stays while no window is open; clicking it opens a window, its menu
//! offers New window / Quit. Notifications use the icon's balloon, which Windows 10/11 show as
//! a regular toast — no packaging / AppUserModelID registration needed.
//!
//! Everything runs on its own thread with a hidden window; the public functions only post to it.

use std::collections::VecDeque;
use std::sync::OnceLock;

use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow,
    TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CONTEXTMENU,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Click on the icon (or "Open" in its menu).
    Open,
    NewWindow,
    /// "Settings" in its menu.
    Settings,
    Quit,
}

/// Menu labels (the app's language).
#[derive(Clone)]
pub struct Labels {
    pub tooltip: String,
    pub open: String,
    pub new_window: String,
    /// Empty: no such item.
    pub settings: String,
    pub quit: String,
}

enum Cmd {
    Show(Labels),
    Hide,
    Notify { title: String, body: String },
}

struct Tray {
    hwnd: isize,
    queue: Mutex<VecDeque<Cmd>>,
    on_event: Mutex<Option<Box<dyn Fn(TrayEvent) + Send>>>,
}

static TRAY: OnceLock<Tray> = OnceLock::new();
const WM_CMD: u32 = WM_APP + 21;
const WM_TRAY: u32 = WM_APP + 22;
const ICON_ID: u32 = 1;

thread_local! {
    /// Thread-side state: whether the icon is shown on request (vs. only for a notification).
    static STATE: std::cell::RefCell<(Option<Labels>, bool)> = const { std::cell::RefCell::new((None, false)) };
}

fn tray() -> &'static Tray {
    TRAY.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        std::thread::Builder::new()
            .name("rf-tray".into())
            .spawn(move || unsafe {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let class = w!("smoothyfiles.Tray");
                let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None)
                    .unwrap_or_default();
                let _ = tx.send(hwnd.0 as isize);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            })
            .expect("spawn tray thread");
        let hwnd = rx.recv().unwrap_or(0);
        Tray { hwnd, queue: Mutex::new(VecDeque::new()), on_event: Mutex::new(None) }
    })
}

fn post(cmd: Cmd) {
    let t = tray();
    t.queue.lock().push_back(cmd);
    unsafe {
        let _ = PostMessageW(Some(HWND(t.hwnd as *mut _)), WM_CMD, WPARAM(0), LPARAM(0));
    }
}

/// Where tray clicks / menu choices go (set once; later calls replace it).
pub fn set_handler(f: impl Fn(TrayEvent) + Send + 'static) {
    *tray().on_event.lock() = Some(Box::new(f));
}

/// Shows (or updates) the icon.
pub fn show(labels: Labels) {
    post(Cmd::Show(labels));
}

/// Removes the icon (a notification still adds it for a moment).
pub fn hide() {
    post(Cmd::Hide);
}

/// A notification (toast) from smoothyfiles. Respects Focus Assist / quiet hours.
pub fn notify(title: &str, body: &str) {
    post(Cmd::Notify { title: title.to_string(), body: body.to_string() });
}

fn copy_into(dst: &mut [u16], s: &str) {
    let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}

fn app_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        // resource id 1 = the app icon (smoothyfiles.rc)
        LoadImageW(Some(hinst.into()), PCWSTR(1 as *const u16), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// Adds the icon if it is not there yet. Returns whether it was added now.
fn ensure_icon(hwnd: HWND, tooltip: &str) -> bool {
    if STATE.with(|s| s.borrow().1) {
        return false;
    }
    let mut nid = base(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = app_icon();
    copy_into(&mut nid.szTip, tooltip);
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
            STATE.with(|s| s.borrow_mut().1 = true);
            return true;
        }
    }
    false
}

fn remove_icon(hwnd: HWND) {
    let nid = base(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    STATE.with(|s| s.borrow_mut().1 = false);
}

fn run_commands(hwnd: HWND) {
    let Some(t) = TRAY.get() else { return };
    loop {
        let cmd = t.queue.lock().pop_front();
        let Some(cmd) = cmd else { break };
        match cmd {
            Cmd::Show(l) => {
                let tip = l.tooltip.clone();
                STATE.with(|s| s.borrow_mut().0 = Some(l));
                if !ensure_icon(hwnd, &tip) {
                    let mut nid = base(hwnd);
                    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
                    copy_into(&mut nid.szTip, &tip);
                    unsafe {
                        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                    }
                }
            }
            Cmd::Hide => {
                STATE.with(|s| s.borrow_mut().0 = None);
                remove_icon(hwnd);
            }
            Cmd::Notify { title, body } => {
                let temporary = ensure_icon(hwnd, "smoothyfiles") && STATE.with(|s| s.borrow().0.is_none());
                let mut nid = base(hwnd);
                nid.uFlags = NIF_INFO;
                nid.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
                copy_into(&mut nid.szInfoTitle, &title);
                copy_into(&mut nid.szInfo, &body);
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                }
                if temporary {
                    // the toast stays in the notification center; the icon goes after a while
                    let h = hwnd.0 as isize;
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(8));
                        unsafe {
                            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CMD, WPARAM(1), LPARAM(0));
                        }
                    });
                }
            }
        }
    }
}

fn fire(e: TrayEvent) {
    if let Some(t) = TRAY.get() {
        if let Some(f) = t.on_event.lock().as_ref() {
            f(e);
        }
    }
}

fn menu(hwnd: HWND) {
    let Some(l) = STATE.with(|s| s.borrow().0.clone()) else { return };
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let ws = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let (a, b, c, d) = (ws(&l.open), ws(&l.new_window), ws(&l.quit), ws(&l.settings));
        let _ = AppendMenuW(m, MF_STRING, 1, PCWSTR(a.as_ptr()));
        // boş etiket: öğe yok (DirectLook'un menüsünde "Yeni pencere" yok)
        if !l.new_window.is_empty() {
            let _ = AppendMenuW(m, MF_STRING, 2, PCWSTR(b.as_ptr()));
        }
        if !l.settings.is_empty() {
            let _ = AppendMenuW(m, MF_STRING, 4, PCWSTR(d.as_ptr()));
        }
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(m, MF_STRING, 3, PCWSTR(c.as_ptr()));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // required so the menu closes when clicking elsewhere
        let _ = SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(m);
        match id.0 {
            1 => fire(TrayEvent::Open),
            2 => fire(TrayEvent::NewWindow),
            3 => fire(TrayEvent::Quit),
            4 => fire(TrayEvent::Settings),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
    let taskbar_created = *TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
    match msg {
        WM_CMD => {
            if wp.0 == 1 {
                // end of a temporary (notification-only) icon
                if STATE.with(|s| s.borrow().0.is_none()) {
                    remove_icon(hwnd);
                }
            } else {
                run_commands(hwnd);
            }
            LRESULT(0)
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in LOWORD(lParam)
            match (lp.0 as u32) & 0xFFFF {
                x if x == WM_LBUTTONUP || x == NIN_SELECT || x == WM_LBUTTONDBLCLK => {
                    if STATE.with(|s| s.borrow().0.is_some()) {
                        fire(TrayEvent::Open);
                    }
                }
                x if x == WM_CONTEXTMENU => menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        m if m == taskbar_created && taskbar_created != 0 => {
            // Explorer restarted: the icon is gone, add it back if it should be there
            STATE.with(|s| s.borrow_mut().1 = false);
            if let Some(l) = STATE.with(|s| s.borrow().0.clone()) {
                ensure_icon(hwnd, &l.tooltip);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
