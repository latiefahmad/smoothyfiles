//! "Safely remove" support, the way Explorer does it.
//!
//! Windows refuses to eject a USB stick (or dismount any volume) while a process holds a handle
//! on it — our folder watcher, an open preview handler, a running size walk. Explorer keeps a
//! handle on every volume root registered with `RegisterDeviceNotification`, so the system
//! asks it first (`DBT_DEVICEQUERYREMOVE`); it then closes its windows on that drive and lets go.
//!
//! This module does the same on its own thread (never the UI or shell STA thread: the app is
//! given a moment to release its handles while the system waits for our answer):
//! - one notification handle per non-system volume, re-synced when drives come and go;
//! - on a removal query: close our handle, call the app, wait (bounded) for its ack, allow;
//! - removal failed (someone else still uses it): register again.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, OPEN_EXISTING,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE, DBT_DEVICEQUERYREMOVEFAILED, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVICEREMOVEPENDING, DBT_DEVTYP_HANDLE, DEV_BROADCAST_HANDLE, DEV_BROADCAST_HDR, DEVICE_NOTIFY_WINDOW_HANDLE,
    DefWindowProcW, DispatchMessageW, GetMessageW, HDEVNOTIFY, MSG, RegisterClassW, RegisterDeviceNotificationW,
    SetTimer, TranslateMessage, UnregisterDeviceNotification, WM_DEVICECHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::wide::wide;

/// Called on the removal thread with the drive letter and an ack sender; send (or drop) the
/// ack once the app has let go of everything on that drive.
type OnQuery = Box<dyn Fn(char, Sender<()>) + Send>;

struct Reg {
    handle: isize,
    notify: isize,
}

#[derive(Default)]
struct State {
    hwnd: isize,
    regs: HashMap<char, Reg>,
    /// Letters released for a pending removal: not re-registered until it completes, fails,
    /// or the grace period passes (re-opening a handle now would block the eject again).
    released: HashMap<char, Instant>,
    /// Volumes that can't be watched (a virtual drive such as Google Drive's refuses the
    /// notification): not tried again while the letter stays, instead of every few seconds.
    refused: std::collections::HashSet<char>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static CALLBACK: OnceLock<Mutex<Option<OnQuery>>> = OnceLock::new();

/// How long the system's removal request waits for the app; the eject proceeds regardless.
const APP_WAIT: Duration = Duration::from_secs(3);
/// Handle closes that happen just after the ack (watcher threads) get this much extra time.
const SETTLE: Duration = Duration::from_millis(300);
const RELEASE_GRACE: Duration = Duration::from_secs(20);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Starts watching (idempotent; the first callback wins).
pub fn start(on_query_remove: impl Fn(char, Sender<()>) + Send + 'static) {
    // (not waited for: opening each drive's root can take a while with a sleeping disk, and the
    // app's start must not wait on it)
    let _ = start_inner(on_query_remove);
}

/// [`start`], with a receiver that gets a message once the drives are registered.
fn start_inner(on_query_remove: impl Fn(char, Sender<()>) + Send + 'static) -> Option<std::sync::mpsc::Receiver<()>> {
    let cb = CALLBACK.get_or_init(|| Mutex::new(None));
    {
        let mut c = cb.lock();
        if c.is_some() {
            return None;
        }
        *c = Some(Box::new(on_query_remove));
    }
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("rf-removal".into()).spawn(move || unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("smoothyfiles.DeviceRemoval");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // A hidden top-level window (not message-only): volume arrival broadcasts only reach those.
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class, w!(""), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None)
            .unwrap_or_default();
        state().lock().hwnd = hwnd.0 as isize;
        sync(hwnd);
        let _ = tx.send(());
        // fallback for drives that appear without a broadcast (and expired grace periods)
        SetTimer(Some(hwnd), 1, 3000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    Some(rx)
}

fn system_drive() -> char {
    std::env::var("SystemDrive").ok().and_then(|s| s.chars().next()).unwrap_or('C').to_ascii_uppercase()
}

/// Registers new volumes, forgets vanished ones.
fn sync(hwnd: HWND) {
    let mask = unsafe { GetLogicalDrives() };
    let sys = system_drive();
    let mut st = state().lock();
    st.released.retain(|l, t| t.elapsed() < RELEASE_GRACE && mask & (1 << (*l as u8 - b'A')) != 0);
    st.refused.retain(|l| mask & (1 << (*l as u8 - b'A')) != 0);
    let gone: Vec<char> = st.regs.keys().copied().filter(|l| mask & (1 << (*l as u8 - b'A')) == 0).collect();
    for l in gone {
        if let Some(r) = st.regs.remove(&l) {
            unregister(r);
        }
    }
    for i in 0..26u8 {
        let l = (b'A' + i) as char;
        if mask & (1 << i) == 0 || l == sys || st.regs.contains_key(&l) || st.released.contains_key(&l) || st.refused.contains(&l) {
            continue;
        }
        match register(hwnd, l) {
            Ok(Some(r)) => {
                st.regs.insert(l, r);
            }
            Ok(None) => {}
            Err(()) => {
                st.refused.insert(l);
            }
        }
    }
}

/// `Ok(None)`: nothing to watch now (not a local disk, an empty card reader: try again later);
/// `Err`: the volume refuses to be watched.
fn register(hwnd: HWND, letter: char) -> Result<Option<Reg>, ()> {
    let root = format!("{letter}:\\");
    let rw = wide(&root);
    // removable, fixed (USB disks report as fixed), optical; not network/RAM/unknown
    let t = unsafe { GetDriveTypeW(PCWSTR(rw.as_ptr())) };
    if !matches!(t, 2 | 3 | 5) {
        return Ok(None);
    }
    unsafe {
        let h = CreateFileW(
            PCWSTR(rw.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        );
        // empty card reader / no disc: try again on the next sync
        let Ok(h) = h else { return Ok(None) };
        let mut filter = DEV_BROADCAST_HANDLE {
            dbch_size: std::mem::size_of::<DEV_BROADCAST_HANDLE>() as u32,
            dbch_devicetype: DBT_DEVTYP_HANDLE.0,
            dbch_handle: h,
            ..Default::default()
        };
        match RegisterDeviceNotificationW(HANDLE(hwnd.0), &mut filter as *mut _ as *const _, DEVICE_NOTIFY_WINDOW_HANDLE) {
            Ok(n) => Ok(Some(Reg { handle: h.0 as isize, notify: n.0 as isize })),
            Err(e) => {
                log::info!("{letter}: can't watch for removal ({e}), not trying again");
                let _ = CloseHandle(h);
                Err(())
            }
        }
    }
}

fn unregister(r: Reg) {
    unsafe {
        let _ = UnregisterDeviceNotification(HDEVNOTIFY(r.notify as *mut _));
        let _ = CloseHandle(HANDLE(r.handle as *mut _));
    }
}

fn letter_for(handle: isize, notify: isize) -> Option<char> {
    state().lock().regs.iter().find(|(_, r)| r.handle == handle || r.notify == notify).map(|(l, _)| *l)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            sync(hwnd);
            LRESULT(0)
        }
        WM_DEVICECHANGE => on_device_change(hwnd, wp.0 as u32, lp),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn on_device_change(hwnd: HWND, event: u32, lp: LPARAM) -> LRESULT {
    let handle_info = || -> Option<(isize, isize)> {
        if lp.0 == 0 {
            return None;
        }
        let hdr = unsafe { &*(lp.0 as *const DEV_BROADCAST_HDR) };
        if hdr.dbch_devicetype != DBT_DEVTYP_HANDLE {
            return None;
        }
        let h = unsafe { &*(lp.0 as *const DEV_BROADCAST_HANDLE) };
        Some((h.dbch_handle.0 as isize, h.dbch_hdevnotify.0 as isize))
    };
    match event {
        DBT_DEVICEQUERYREMOVE => {
            if let Some(letter) = handle_info().and_then(|(h, n)| letter_for(h, n)) {
                {
                    let mut st = state().lock();
                    if let Some(r) = st.regs.remove(&letter) {
                        unregister(r);
                    }
                    st.released.insert(letter, Instant::now());
                }
                let (tx, rx) = std::sync::mpsc::channel();
                if let Some(cb) = CALLBACK.get().and_then(|c| c.lock().as_ref().map(|f| f as *const OnQuery)) {
                    // SAFETY: the callback is never replaced or dropped once set
                    unsafe { (*cb)(letter, tx) };
                    if rx.recv_timeout(APP_WAIT).is_ok() {
                        std::thread::sleep(SETTLE);
                    }
                }
            }
            LRESULT(1) // allow
        }
        DBT_DEVICEQUERYREMOVEFAILED => {
            // someone else still holds the drive: it stays, watch it again
            state().lock().released.clear();
            sync(hwnd);
            LRESULT(1)
        }
        DBT_DEVICEREMOVECOMPLETE | DBT_DEVICEREMOVEPENDING | DBT_DEVICEARRIVAL => {
            sync(hwnd);
            LRESULT(1)
        }
        _ => LRESULT(1),
    }
}

/// Drive letters currently registered (for tests and diagnostics).
pub fn registered() -> Vec<char> {
    let mut v: Vec<char> = state().lock().regs.keys().copied().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    /// Simulates the system's removal query for one registered volume: the app callback runs,
    /// our handle is closed, and a failed removal registers the volume again.
    #[test]
    fn query_remove_releases_and_failed_removal_reregisters() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<char>();
        let seen_tx = Mutex::new(seen_tx);
        if let Some(ready) = start_inner(move |l, ack| {
            let _ = seen_tx.lock().send(l);
            let _ = ack.send(());
        }) {
            let _ = ready.recv_timeout(Duration::from_secs(5));
        }
        let regs = registered();
        let Some(&letter) = regs.first() else {
            eprintln!("no non-system volume on this machine; skipped");
            return;
        };
        let (hwnd, handle, notify) = {
            let st = state().lock();
            let r = &st.regs[&letter];
            (st.hwnd, r.handle, r.notify)
        };
        let mut msg = DEV_BROADCAST_HANDLE {
            dbch_size: std::mem::size_of::<DEV_BROADCAST_HANDLE>() as u32,
            dbch_devicetype: DBT_DEVTYP_HANDLE.0,
            dbch_handle: HANDLE(handle as *mut _),
            dbch_hdevnotify: HDEVNOTIFY(notify as *mut _),
            ..Default::default()
        };
        let hwnd = HWND(hwnd as *mut _);
        let r = unsafe {
            SendMessageW(hwnd, WM_DEVICECHANGE, Some(WPARAM(DBT_DEVICEQUERYREMOVE as usize)), Some(LPARAM(&mut msg as *mut _ as isize)))
        };
        assert_eq!(r.0, 1, "removal allowed");
        assert_eq!(seen_rx.recv_timeout(Duration::from_secs(1)).unwrap(), letter);
        assert!(!registered().contains(&letter), "handle released");
        // the periodic sync must not grab it again while the removal is pending
        unsafe { SendMessageW(hwnd, WM_TIMER, Some(WPARAM(1)), Some(LPARAM(0))) };
        assert!(!registered().contains(&letter));
        unsafe { SendMessageW(hwnd, WM_DEVICECHANGE, Some(WPARAM(DBT_DEVICEQUERYREMOVEFAILED as usize)), Some(LPARAM(&mut msg as *mut _ as isize))) };
        assert!(registered().contains(&letter), "watched again after a failed removal");
    }
}
