//! A long-lived single-threaded-apartment "shell thread" with a message pump and a hidden
//! owner window. Context menus, property sheets and other modeless shell UI must live on a
//! thread that keeps pumping messages — and must never be the UI thread, so a slow shell
//! extension can't freeze smoothyfiles.

use std::collections::VecDeque;
use std::sync::OnceLock;

use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WM_APP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

type Job = Box<dyn FnOnce(HWND) + Send>;

struct ShellThread {
    hwnd: isize,
    queue: Mutex<VecDeque<Job>>,
}

static THREAD: OnceLock<ShellThread> = OnceLock::new();
const WM_RUN: u32 = WM_APP + 1;

thread_local! {
    /// Window-message hook installed while a context menu is open (IContextMenu2/3 forwarding).
    pub(crate) static MENU_HOOK: std::cell::RefCell<Option<Box<dyn Fn(u32, WPARAM, LPARAM) -> Option<LRESULT>>>> =
        const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_RUN {
        drain(hwnd);
        return LRESULT(0);
    }
    let hooked = MENU_HOOK.with(|h| h.borrow().as_ref().and_then(|f| f(msg, wp, lp)));
    if let Some(r) = hooked {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn drain(hwnd: HWND) {
    let Some(t) = THREAD.get() else { return };
    loop {
        let job = t.queue.lock().pop_front();
        match job {
            Some(j) => j(hwnd),
            None => break,
        }
    }
}

fn start() -> ShellThread {
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    std::thread::Builder::new()
        .name("rf-shell-sta".into())
        .spawn(move || unsafe {
            let _ = OleInitialize(None);
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let class = w!("smoothyfiles.ShellOwner");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .unwrap_or_default();
            let _ = tx.send(hwnd.0 as isize);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("spawn shell thread");
    let hwnd = rx.recv().unwrap_or(0);
    ShellThread { hwnd, queue: Mutex::new(VecDeque::new()) }
}

/// Runs `f` on the shell STA thread. `f` receives the hidden owner window.
pub fn run(f: impl FnOnce(HWND) + Send + 'static) {
    let t = THREAD.get_or_init(start);
    t.queue.lock().push_back(Box::new(f));
    unsafe {
        let _ = PostMessageW(Some(HWND(t.hwnd as *mut _)), WM_RUN, WPARAM(0), LPARAM(0));
    }
}

#[allow(dead_code)]
fn _unused(_: WINDOW_EX_STYLE) {}
