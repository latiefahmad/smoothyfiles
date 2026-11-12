//! Önizleme penceresi için Win32 yardımcıları (üstte tut, konum, görev çubuğu, çalışma alanı).

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GWL_EXSTYLE, GWLP_HWNDPARENT, GetCursorPos, GetWindowLongPtrW, HWND_NOTOPMOST, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WINDOW_STYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::PCWSTR;

fn h(hwnd: isize) -> HWND {
    HWND(hwnd as *mut _)
}

fn work_of(mon: windows::Win32::Graphics::Gdi::HMONITOR) -> [f32; 4] {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let (mut dx, mut dy) = (96u32, 96u32);
    unsafe {
        let _ = GetMonitorInfoW(mon, &mut mi);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    }
    let s = dx.max(1) as f32 / 96.0;
    let r = mi.rcWork;
    [r.left as f32 / s, r.top as f32 / s, (r.right - r.left) as f32 / s, (r.bottom - r.top) as f32 / s]
}

/// İmlecin bulunduğu monitörün çalışma alanı, GPUI'nin mantıksal koordinatlarında (x, y, g, y).
pub fn work_area_at_cursor() -> [f32; 4] {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
        work_of(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST))
    }
}

/// Pencerenin bulunduğu monitörün çalışma alanı (mantıksal).
pub fn work_area_of(hwnd: isize) -> [f32; 4] {
    unsafe { work_of(MonitorFromWindow(h(hwnd), MONITOR_DEFAULTTONEAREST)) }
}

/// Başka bir sürecin (ör. DirectLook'tan çağrılan smoothyfiles) pencereyi öne alabilmesine izin verir;
/// yoksa Windows ön plan kilidi yüzünden pencere yalnız görev çubuğunda yanıp söner.
pub fn allow_any_foreground() {
    use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

pub fn set_topmost(hwnd: isize, on: bool) {
    unsafe {
        let _ = SetWindowPos(h(hwnd), Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// Pencereyi mantıksal dikdörtgene taşır (`scale`: pencerenin DPI ölçeği).
pub fn set_rect(hwnd: isize, rect: [f32; 4], scale: f32) {
    let p = |v: f32| (v * scale).round() as i32;
    unsafe {
        let _ = SetWindowPos(h(hwnd), None, p(rect[0]), p(rect[1]), p(rect[2]), p(rect[3]), SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

thread_local! {
    /// Görev çubuğunda görünmeyecek pencerelerin gizli sahibi (pencerenin iş parçacığında yaratılır).
    static OWNER: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

fn hidden_owner() -> isize {
    OWNER.with(|o| {
        if o.get() == 0 {
            let class = crate::wide::wide("STATIC");
            let hw = unsafe {
                CreateWindowExW(
                    Default::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR::null(),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if let Ok(hw) = hw {
                o.set(hw.0 as isize);
            }
        }
        o.get()
    })
}

/// Görev çubuğunda görünsün mü. Gizlemek için pencereye gizli bir sahip verilir (araç penceresi
/// stili Windows'un küçük kapat düğmesini başlık çubuğunun altında belirginleştiriyor).
pub fn set_show_in_taskbar(hwnd: isize, show: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(h(hwnd), GWL_EXSTYLE) & !(WS_EX_TOOLWINDOW.0 as isize);
        if show {
            SetWindowLongPtrW(h(hwnd), GWLP_HWNDPARENT, 0);
            SetWindowLongPtrW(h(hwnd), GWL_EXSTYLE, ex | WS_EX_APPWINDOW.0 as isize);
        } else {
            let owner = hidden_owner();
            SetWindowLongPtrW(h(hwnd), GWL_EXSTYLE, ex & !(WS_EX_APPWINDOW.0 as isize));
            if owner != 0 {
                SetWindowLongPtrW(h(hwnd), GWLP_HWNDPARENT, owner);
            }
        }
        let _ = SetWindowPos(h(hwnd), None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    }
}

/// Bu adla çalışan bir süreç var mı (ör. "QuickLook.exe"; büyük/küçük harf duyarsız).
pub fn process_running(exe: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(exe) {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_own_process() {
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().into_owned();
        assert!(super::process_running(&name));
        assert!(!super::process_running("bu-surec-yok-123.exe"));
    }
}
