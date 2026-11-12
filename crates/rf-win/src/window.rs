//! Small top-level window helpers that GPUI does not expose.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    WS_EX_TOPMOST,
};

/// Keeps the window above all non-topmost windows ("compact overlay" / always on top).
pub fn set_topmost(hwnd: isize, on: bool) -> windows::core::Result<()> {
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe { SetWindowPos(HWND(hwnd as *mut _), Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) }
}

pub fn is_topmost(hwnd: isize) -> bool {
    let ex = unsafe { GetWindowLongPtrW(HWND(hwnd as *mut _), GWL_EXSTYLE) };
    ex as u32 & WS_EX_TOPMOST.0 != 0
}

/// Whether the foreground window belongs to this process.
pub fn app_in_front() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

/// Window rectangle in physical pixels `[left, top, right, bottom]`, and whether it is maximized.
pub fn rect(hwnd: isize) -> Option<([i32; 4], bool)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsZoomed};
    let h = HWND(hwnd as *mut _);
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    Some(([r.left, r.top, r.right, r.bottom], unsafe { IsZoomed(h).as_bool() }))
}

/// Restores (un-maximizes) and moves/resizes the window, physical pixels.
pub fn place(hwnd: isize, r: [i32; 4]) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SWP_NOZORDER, ShowWindow};
    let h = HWND(hwnd as *mut _);
    unsafe {
        let _ = ShowWindow(h, SW_RESTORE);
        let _ = SetWindowPos(h, None, r[0], r[1], r[2] - r[0], r[3] - r[1], SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn maximize(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_MAXIMIZE, ShowWindow};
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_MAXIMIZE);
    }
}

/// Overrides the minimum window size (logical pixels) at runtime, `None` = back to the
/// size given at creation. Read by our GPUI fork on WM_GETMINMAXINFO.
pub fn set_min_size(hwnd: isize, size: Option<(u16, u16)>) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::UI::WindowsAndMessaging::{RemovePropW, SetPropW};
    let h = HWND(hwnd as *mut _);
    let name = windows::core::w!("smoothyfiles.MinSize");
    unsafe {
        match size {
            Some((w, hh)) => {
                let v = ((w as usize) << 16) | hh as usize;
                let _ = SetPropW(h, name, Some(HANDLE(v as *mut _)));
            }
            None => {
                let _ = RemovePropW(h, name);
            }
        }
    }
}

/// Dark or light window frame / system backdrop (Mica follows it), independent of the system
/// setting — so the app's own Light / Dark choice also tints Mica.
/// The process's AppUserModelID: windows group under, and pins match, the shortcuts carrying
/// the same id (not the exe path, which changes folder on every update).
pub fn set_app_id(id: &str) {
    let w = crate::wide::wide(id);
    if let Err(e) = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::PCWSTR(w.as_ptr())) } {
        log::warn!("app id: {e}");
    }
}

pub fn set_dark_mode(hwnd: isize, dark: bool) {
    use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let v: i32 = dark as i32;
    unsafe {
        let _ = DwmSetWindowAttribute(HWND(hwnd as *mut _), DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const _ as _, std::mem::size_of::<i32>() as u32);
    }
}

/// Makes `owner` (a window of another process, possibly) the owner of `hwnd`: it stays above
/// it and minimizes with it, like the owner's own dialog.
pub fn set_owner(hwnd: isize, owner: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GWLP_HWNDPARENT, SetWindowLongPtrW};
    unsafe {
        SetWindowLongPtrW(HWND(hwnd as *mut _), GWLP_HWNDPARENT, owner);
    }
}

/// Puts `hwnd` above the window that owns it, without activating anything.
///
/// An owner set after the window exists (`set_owner`) does not move it: our picker could be the
/// window in front and still lie under the app that asked for it (seen with WhatsApp: you click
/// the disabled app, it beeps, and only then does the picker come up). Raised to the top this way
/// (no SWP_NOOWNERZORDER), Windows places the owner right under it. Moving the owner waits for
/// the owner's thread: call it on a thread of its own, so a hung app can't hold ours.
pub fn raise_above_owner(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos};
    unsafe {
        let _ = SetWindowPos(HWND(hwnd as *mut _), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Gives the foreground back to `owner` (another app's window) while we still hold it: when our
/// window closes, Windows would otherwise activate whatever window comes next.
pub fn hand_foreground(owner: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow};
    unsafe {
        let h = HWND(owner as *mut _);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        let _ = SetForegroundWindow(h);
    }
}

/// Brings `hwnd` to the front and gives it the keyboard (the caller was allowed to by the app
/// in front, see AllowSetForegroundWindow).
///
/// When that permission didn't come (Chrome asks from a background helper process, which
/// can't grant it), the window would open behind the app: then our thread shares the input
/// state of the window in front for a moment, which lets the switch through.
pub fn bring_to_front(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SW_SHOW, SetForegroundWindow, ShowWindow};
    unsafe {
        let h = HWND(hwnd as *mut _);
        let _ = ShowWindow(h, SW_SHOW);
        if SetForegroundWindow(h).as_bool() && GetForegroundWindow() == h {
            return;
        }
        let front = GetForegroundWindow();
        let theirs = GetWindowThreadProcessId(front, None);
        let mine = GetCurrentThreadId();
        let attached = theirs != 0 && theirs != mine && AttachThreadInput(mine, theirs, true).as_bool();
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        if attached {
            let _ = AttachThreadInput(mine, theirs, false);
        }
        if GetForegroundWindow() != h {
            // Windows lifts the foreground lock while Alt is down (how Alt+Tab works): a tap of
            // it, then the switch (GPUI's activate does the same)
            use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_MENU};
            let key = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_MENU, dwFlags: flags, ..Default::default() } } };
            let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(h);
            let _ = BringWindowToTop(h);
        }
        if GetForegroundWindow() != h {
            // no keyboard focus granted: at least on top of every other window
            use windows::Win32::UI::WindowsAndMessaging::{HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos};
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
            let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(h, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        }
    }
}

/// Whether `hwnd` is the window in front (has the keyboard).
pub fn is_foreground(hwnd: isize) -> bool {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == hwnd }
}

/// The colour on screen down a column of `hwnd`'s client area: `x` and `y0..y1` in client
/// pixels. The median of each channel, so icons and markers crossing the column don't count.
/// What the user sees there, Mica's wallpaper tint included, which the app can't know otherwise.
pub fn screen_color(hwnd: isize, x: i32, y0: i32, y1: i32) -> Option<[u8; 3]> {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
    };
    let h = (y1 - y0).max(0);
    if h == 0 {
        return None;
    }
    unsafe {
        let mut p = POINT { x, y: y0 };
        if !ClientToScreen(HWND(hwnd as *mut _), &mut p).as_bool() {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, 1, h);
        let old = SelectObject(mem, bmp.into());
        let copied = BitBlt(mem, 0, 0, 1, h, Some(screen), p.x, p.y, SRCCOPY).is_ok();
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: 1, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
            ..Default::default()
        };
        let mut px = vec![0u32; h as usize];
        let rows = if copied { GetDIBits(mem, bmp, 0, h as u32, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS) } else { 0 };
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if rows <= 0 {
            return None;
        }
        let median = |shift: u32| {
            let mut v: Vec<u8> = px.iter().map(|c| (c >> shift) as u8).collect();
            v.sort_unstable();
            v[v.len() / 2]
        };
        Some([median(16), median(8), median(0)])
    }
}

/// A window for a log line: handle, process, class and title.
pub fn describe(hwnd: isize) -> String {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW, GetWindowThreadProcessId};
    if hwnd == 0 {
        return "none".into();
    }
    unsafe {
        let h = HWND(hwnd as *mut _);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let exe = crate::tasklist::exe_of(pid).and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "?".into());
        let mut class = [0u16; 128];
        let n = GetClassNameW(h, &mut class);
        let mut title = [0u16; 128];
        let t = GetWindowTextW(h, &mut title);
        format!(
            "0x{hwnd:x} {exe}[{pid}] {} '{}'",
            String::from_utf16_lossy(&class[..n.max(0) as usize]),
            String::from_utf16_lossy(&title[..t.max(0) as usize])
        )
    }
}

/// The window that owns `hwnd` (0 when none).
pub fn owner_of(hwnd: isize) -> isize {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GW_OWNER, GetWindow};
    unsafe { GetWindow(HWND(hwnd as *mut _), GW_OWNER).map_or(0, |h| h.0 as isize) }
}

/// Whether `a` lies above `b` in the z-order (`None` when either isn't a top-level window).
pub fn is_above(a: isize, b: isize) -> Option<bool> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GW_HWNDNEXT, GetTopWindow, GetWindow};
    unsafe {
        let mut h = GetTopWindow(None).ok()?;
        for _ in 0..10_000 {
            let v = h.0 as isize;
            if v == a {
                return Some(true);
            }
            if v == b {
                return Some(false);
            }
            h = GetWindow(HWND(h.0), GW_HWNDNEXT).ok()?;
        }
        None
    }
}

/// Whether `hwnd` is still a window.
pub fn exists(hwnd: isize) -> bool {
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(windows::Win32::Foundation::HWND(hwnd as *mut _))).as_bool() }
}

/// The window in front (0 when none).
pub fn foreground() -> isize {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
}
