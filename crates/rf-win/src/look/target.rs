//! Ön plandaki pencerenin türü (Explorer, masaüstü, iletişim kutusu…) ve içindeki seçili öğe.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IDataObject, IDispatch, IServiceProvider};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IShellBrowser, IShellItemArray, IShellWindows, SHCreateShellItemArrayFromDataObject, SID_STopLevelBrowser,
    SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, SVGIO_SELECTION, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GA_ROOT, GUI_CARETBLINKING, GUI_INMENUMODE, GUI_POPUPMENUMODE, GUITHREADINFO, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};
use windows::core::{Interface, PCWSTR};

use crate::wide::wide;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind { Explorer, Desktop, Dialog, Everything, DirectoryOpus, None }

/// Pencere sınıfı ve içeriğinden hedef türü (saf).
pub fn classify(class: &str, _title: &str, has_shell_view: bool, has_dui_view: bool) -> TargetKind {
    match class {
        "CabinetWClass" | "ExploreWClass" => TargetKind::Explorer,
        "Progman" | "WorkerW" if has_shell_view => TargetKind::Desktop,
        "#32770" if has_dui_view => TargetKind::Dialog,
        "dopus.lister" => TargetKind::DirectoryOpus,
        c if c.starts_with("EVERYTHING") => TargetKind::Everything,
        _ => TargetKind::None,
    }
}

pub(crate) fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn child(parent: HWND, class: &str) -> Option<HWND> {
    let c = wide(class);
    unsafe { FindWindowExW(Some(parent), None, PCWSTR(c.as_ptr()), PCWSTR::null()) }.ok().filter(|h| !h.is_invalid())
}

/// Ön plandaki pencere ve türü.
pub fn foreground_target() -> (TargetKind, isize) {
    let h = unsafe { GetForegroundWindow() };
    if h.is_invalid() {
        return (TargetKind::None, 0);
    }
    let class = class_of(h);
    let shell_view = matches!(class.as_str(), "Progman" | "WorkerW") && child(h, "SHELLDLL_DefView").is_some();
    let dui = class == "#32770" && child(h, "DUIViewWndClassName").is_some();
    // başlık okunmaz: GetWindowTextW kendi sürecimizin penceresine mesaj gönderir (kancada beklenmemeli)
    (classify(&class, "", shell_view, dui), h.0 as isize)
}

/// Pencerenin iş parçacığında yazı yazılıyor mu (imleç var, menü açık ya da Explorer arama kutusu odakta).
pub fn is_typing(hwnd: isize) -> bool {
    let h = HWND(hwnd as *mut _);
    let tid = unsafe { GetWindowThreadProcessId(h, None) };
    if tid == 0 {
        return false;
    }
    let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    if unsafe { GetGUIThreadInfo(tid, &mut info) }.is_err() {
        return false;
    }
    if !info.hwndCaret.is_invalid() || (info.flags.0 & (GUI_CARETBLINKING.0 | GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0)) != 0 {
        return true;
    }
    !info.hwndFocus.is_invalid() && class_of(info.hwndFocus) == "Windows.UI.Core.CoreWindow"
}

fn browser_of(disp: &IDispatch) -> Option<IShellBrowser> {
    let sp: IServiceProvider = disp.cast().ok()?;
    unsafe { sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) }.ok()
}

fn explorer_browser(windows: &IShellWindows, hwnd: HWND) -> Option<IShellBrowser> {
    // Windows 11 sekmeleri: etkin sekme, en üstteki ShellTabWindowClass çocuğudur.
    let tab = child(hwnd, "ShellTabWindowClass");
    let count = unsafe { windows.Count() }.ok()?;
    let mut fallback = None;
    for i in 0..count {
        let Ok(disp) = (unsafe { windows.Item(&VARIANT::from(i)) }) else { continue };
        let Some(b) = browser_of(&disp) else { continue };
        let Ok(bh) = (unsafe { b.GetWindow() }) else { continue };
        if Some(bh) == tab {
            return Some(b);
        }
        if bh == hwnd || unsafe { GetAncestor(bh, GA_ROOT) } == hwnd {
            fallback.get_or_insert(b);
        }
    }
    fallback
}

fn desktop_browser(windows: &IShellWindows) -> Option<IShellBrowser> {
    let loc = VARIANT::from(CSIDL_DESKTOP as i32);
    let empty = VARIANT::default();
    let mut h = 0i32;
    let disp = unsafe { windows.FindWindowSW(&loc, &empty, SWC_DESKTOP, &mut h, SWFO_NEEDDISPATCH) }.ok()?;
    browser_of(&disp)
}

/// Explorer penceresinde ya da masaüstünde seçili ilk öğenin yolu. Çağıran iş parçacığı COM başlatmış olmalı.
pub fn selected_path(kind: TargetKind, hwnd: isize) -> Option<PathBuf> {
    let windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;
    let browser = match kind {
        TargetKind::Explorer => explorer_browser(&windows, HWND(hwnd as *mut _))?,
        TargetKind::Desktop => desktop_browser(&windows)?,
        _ => return None,
    };
    let view = unsafe { browser.QueryActiveShellView() }.ok()?;
    let data: IDataObject = unsafe { view.GetItemObject(SVGIO_SELECTION) }.ok()?;
    let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromDataObject(&data) }.ok()?;
    if unsafe { items.GetCount() }.ok()? == 0 {
        return None;
    }
    let item = unsafe { items.GetItemAt(0) }.ok()?;
    for sigdn in [SIGDN_FILESYSPATH, SIGDN_DESKTOPABSOLUTEPARSING] {
        if let Ok(p) = unsafe { item.GetDisplayName(sigdn) } {
            let s = unsafe { p.to_string() }.ok();
            unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
            if let Some(s) = s.filter(|s| !s.is_empty()) {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_windows() {
        assert_eq!(classify("CabinetWClass", "Belgeler", false, false), TargetKind::Explorer);
        assert_eq!(classify("Progman", "", true, false), TargetKind::Desktop);
        assert_eq!(classify("WorkerW", "", true, false), TargetKind::Desktop);
        assert_eq!(classify("WorkerW", "", false, false), TargetKind::None);
        assert_eq!(classify("#32770", "Farklı Kaydet", false, true), TargetKind::Dialog);
        assert_eq!(classify("dopus.lister", "", false, false), TargetKind::DirectoryOpus);
        assert_eq!(classify("EVERYTHING", "", false, false), TargetKind::Everything);
        assert_eq!(classify("Notepad", "", false, false), TargetKind::None);
    }

    /// Geçici bir klasörde Explorer açar, bir dosyayı seçtirir ve seçimi okur; pencereyi kapatır.
    #[test]
    #[ignore = "opens an Explorer window on a scratch folder"]
    fn reads_explorer_selection() {
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        crate::shell::ensure_com();
        let dir = std::env::temp_dir().join(format!("rf-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        std::process::Command::new("explorer.exe").arg(format!("/select,{}", file.display())).spawn().unwrap();
        let start = std::time::Instant::now();
        let mut got = None;
        while start.elapsed().as_secs() < 15 && got.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let (kind, h) = foreground_target();
            if kind == TargetKind::Explorer && selected_path(kind, h).as_deref() == Some(file.as_path()) {
                got = Some(h);
            }
        }
        let h = got.expect("Explorer selection not read");
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, Default::default(), Default::default());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
