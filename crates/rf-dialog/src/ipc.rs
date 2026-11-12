//! Inside the calling app: hand the request to smoothyfiles and wait for the answer without
//! freezing the app (its messages keep being pumped while the user picks).

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ASFW_ANY, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostQuitMessage, QS_ALLINPUT,
    TranslateMessage, WM_QUIT,
};
use windows::core::{HSTRING, PCWSTR};

use crate::proto::{Request, Response};

/// How long smoothyfiles may take to pick up the request (a cold start is well under a second).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The folder this DLL was loaded from (smoothyfiles.exe sits next to it).
fn own_dir() -> Option<PathBuf> {
    unsafe {
        let mut m = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(own_dir as *const c_void as *const u16),
            &mut m,
        )
        .ok()?;
        let mut buf = vec![0u16; 32768];
        let n = GetModuleFileNameW(Some(m), &mut buf) as usize;
        if n == 0 {
            return None;
        }
        PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(|p| p.to_path_buf())
    }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Waits for `event` while dispatching the thread's messages. `false` on timeout or WM_QUIT.
fn pump_until(event: HANDLE, timeout: Option<Duration>) -> bool {
    let start = Instant::now();
    loop {
        let left = timeout.map(|t| t.saturating_sub(start.elapsed()));
        if left == Some(Duration::ZERO) {
            return false;
        }
        let ms = left.map_or(u32::MAX, |l| l.as_millis().min(u32::MAX as u128 - 1) as u32);
        let r = unsafe { MsgWaitForMultipleObjectsEx(Some(&[event]), ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if r == WAIT_OBJECT_0 {
            return true;
        }
        if r.0 == WAIT_OBJECT_0.0 + 1 {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        // the app is quitting: give the message back and stop waiting
                        PostQuitMessage(msg.wParam.0 as i32);
                        return false;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            continue;
        }
        return false; // timeout or failure
    }
}

/// Overlapped read/write of exactly `buf.len()` bytes, pumping messages meanwhile.
fn io(pipe: HANDLE, event: HANDLE, buf: &mut [u8], write: bool) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        let mut n = 0u32;
        let r = unsafe {
            if write {
                WriteFile(pipe, Some(&buf[done..]), Some(&mut n), Some(&mut ov))
            } else {
                ReadFile(pipe, Some(&mut buf[done..]), Some(&mut n), Some(&mut ov))
            }
        };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                if !pump_until(event, None) {
                    return false;
                }
                if unsafe { GetOverlappedResult(pipe, &ov, &mut n, false) }.is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
        if n == 0 {
            return false;
        }
        done += n as usize;
    }
    true
}

/// Asks smoothyfiles. `None`: it couldn't be reached or broke off (the caller shows Windows'
/// own dialog instead).
pub fn pick(req: &Request, owner: HWND) -> Option<Response> {
    static N: AtomicU32 = AtomicU32::new(0);
    let name = format!(r"\\.\pipe\smoothyfiles-pick-{}-{}", unsafe { GetCurrentProcessId() }, N.fetch_add(1, Ordering::Relaxed));
    // next to the DLL (a dev build), or where the sidecar file says (the registered copy)
    let dir = own_dir()?;
    let exe = Some(dir.join("smoothyfiles.exe"))
        .filter(|p| p.exists())
        .or_else(|| std::fs::read_to_string(dir.join("smoothyfiles-exe.txt")).ok().map(|s| PathBuf::from(s.trim())).filter(|p| p.exists()));
    let Some(exe) = exe else {
        crate::log(&format!("pick: no smoothyfiles.exe found from {}", dir.display()));
        return None;
    };
    unsafe {
        let pipe = Handle(
            CreateNamedPipeW(&HSTRING::from(name.as_str()), PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 64 << 10, 64 << 10, 0, None),
        );
        if pipe.0.is_invalid() {
            return None;
        }
        let event = Handle(CreateEventW(None, true, false, None).ok()?);
        // smoothyfiles' window has to come to the front over this app
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        if let Err(e) = std::process::Command::new(&exe).args(["--pick", &name]).spawn() {
            crate::log(&format!("pick: can't start {}: {e}", exe.display()));
            return None;
        }

        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        match ConnectNamedPipe(pipe.0, Some(&mut ov)) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                if !pump_until(event.0, Some(CONNECT_TIMEOUT)) {
                    crate::log("pick: smoothyfiles didn't connect");
                    return None;
                }
            }
            Err(_) => return None,
        }
        let _ = owner;
        let mut out = Vec::new();
        crate::proto::write_msg(&mut out, req).ok()?;
        if !io(pipe.0, event.0, &mut out, true) {
            return None;
        }
        let mut len = [0u8; 4];
        if !io(pipe.0, event.0, &mut len, false) {
            crate::log("pick: the answer didn't arrive (pipe closed)");
            return None;
        }
        let mut body = vec![0u8; u32::from_le_bytes(len).min(16 << 20) as usize];
        crate::log(&format!("pick: answer of {} bytes", body.len()));
        if !io(pipe.0, event.0, &mut body, false) {
            return None;
        }
        serde_json::from_slice(&body).ok()
    }
}
