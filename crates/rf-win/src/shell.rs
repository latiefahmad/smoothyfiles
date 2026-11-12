//! Shell actions (open, properties). Each call initializes COM on the calling thread,
//! so run them on a worker thread — shell extensions can block for seconds.

use std::path::Path;

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

use crate::wide::wide;

/// Initializes an STA on the current thread (idempotent; errors ignored when already initialized).
pub fn ensure_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
}

fn exec(path: &Path, verb: Option<&str>, args: Option<&str>) -> anyhow::Result<()> {
    ensure_com();
    let file = wide(path);
    let verb_w = verb.map(wide);
    let args_w = args.map(wide);
    let dir = path.parent().map(wide);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb_w.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: args_w.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
        lpDirectory: dir.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info)? };
    Ok(())
}

/// Opens a file with its default handler (same as double-click in Explorer).
pub fn open(path: &Path) -> anyhow::Result<()> {
    exec(path, None, None)
}

/// Shows the shell "Open with" dialog.
pub fn open_with(path: &Path) -> anyhow::Result<()> {
    exec(path, Some("openas"), None)
}

/// Shows the Windows properties dialog for the item.
pub fn properties(path: &Path) -> anyhow::Result<()> {
    exec(path, Some("properties"), None)
}

/// Opens a terminal (Windows Terminal if present, else PowerShell) in `dir`.
pub fn open_terminal(dir: &Path) -> anyhow::Result<()> {
    let d = dir.to_string_lossy().to_string();
    let wt = std::process::Command::new("wt.exe").args(["-d", &d]).spawn();
    if wt.is_err() {
        std::process::Command::new("powershell.exe").current_dir(dir).spawn()?;
    }
    Ok(())
}

/// Yes/No warning box owned by `owner` (call on the STA shell thread). True on "Yes".
pub fn confirm(owner: windows::Win32::Foundation::HWND, title: &str, text: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, MessageBoxW};
    let t = crate::wide::wide(title);
    let m = crate::wide::wide(text);
    unsafe {
        MessageBoxW(
            Some(owner),
            windows::core::PCWSTR(m.as_ptr()),
            windows::core::PCWSTR(t.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

/// Information box owned by `owner` (call on the STA shell thread).
pub fn alert(owner: windows::Win32::Foundation::HWND, title: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    let t = crate::wide::wide(title);
    let m = crate::wide::wide(text);
    unsafe {
        MessageBoxW(Some(owner), windows::core::PCWSTR(m.as_ptr()), windows::core::PCWSTR(t.as_ptr()), MB_OK | MB_ICONINFORMATION);
    }
}

/// `shell:Downloads`, `shell:startup`, `shell:sendto`… → file-system path (None for virtual
/// folders like `shell:RecycleBinFolder`). Blocking (shell namespace): off the UI thread.
pub fn resolve_shell_path(s: &str) -> Option<std::path::PathBuf> {
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};
    ensure_com();
    let w = crate::wide::wide(s.trim());
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(windows::core::PCWSTR(w.as_ptr()), None).ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let out = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        out.map(std::path::PathBuf::from)
    }
}

#[cfg(test)]
mod shell_path_tests {
    #[test]
    fn resolves_known_shell_paths() {
        let p = super::resolve_shell_path("shell:Windows").expect("shell:Windows");
        assert!(p.to_string_lossy().to_lowercase().ends_with("windows"), "{p:?}");
        assert!(super::resolve_shell_path("shell:RecycleBinFolder").is_none());
    }
}

/// No "There is no disk in the drive" / "Insert a disk" system boxes when touching an empty
/// card reader or an ejected disc: such calls just fail (and the app shows "drive not ready").
/// Process-wide; call once at startup.
pub fn suppress_critical_error_dialogs() {
    use windows::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetErrorMode};
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
    }
}

/// Tells the shell (Explorer windows, desktop, file dialogs) that `dir`'s contents changed,
/// the way the shell's own copy engine does when it finishes.
pub fn notify_updated(dir: &std::path::Path) {
    use windows::Win32::UI::Shell::{SHCNE_UPDATEDIR, SHCNF_PATHW, SHChangeNotify};
    let w = crate::wide::wide(dir);
    // SAFETY: a NUL-terminated wide path, read during the call
    unsafe { SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW, Some(w.as_ptr() as *const _), None) };
}

/// Explorer's "Change..." next to "Opens with": the shell's app chooser for `path`, which on OK makes the
/// chosen app the default for the extension (OAIF_REGISTER_EXT + OAIF_FORCE_REGISTRATION) without opening the
/// file (no OAIF_EXEC). Modal: run it on the STA shell thread.
pub fn open_with_dialog(owner: windows::Win32::Foundation::HWND, path: &Path) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::{OAIF_ALLOW_REGISTRATION, OAIF_FORCE_REGISTRATION, OAIF_REGISTER_EXT, OPENASINFO, SHOpenWithDialog};
    ensure_com();
    let file = wide(path);
    let info = OPENASINFO { pcszFile: PCWSTR(file.as_ptr()), pcszClass: PCWSTR::null(), oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_FORCE_REGISTRATION };
    // SAFETY: info and the path buffer outlive the modal call
    unsafe { SHOpenWithDialog(Some(owner), &info)? };
    Ok(())
}
