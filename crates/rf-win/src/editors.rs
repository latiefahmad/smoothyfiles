//! Installed editors for "Open in editor" (Settings › Developer tools) and "Edit in Notepad"
//! (context menu). Detection: the well-known install folders of the VS Code family, Notepad++'s
//! own registry key, then the usual install roots as a fallback. Blocking disk/registry work:
//! call off the UI thread — [`detect`] on a worker thread, [`cached`] (the startup-warmed copy)
//! while rendering.

use std::path::{Path, PathBuf};

use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::PCWSTR;

/// One installed editor that can open a folder (as a workspace). Notepad can't, and is not
/// listed here — it is only offered for single files, see [`notepad`].
#[derive(Clone)]
pub struct Editor {
    /// Stable id kept in settings (`editor_id`); empty there means "the first of this list".
    pub id: &'static str,
    /// Shown as-is in Settings: a product name needs no translation.
    pub name: &'static str,
    pub exe: PathBuf,
}

/// The editors looked for, in the order "automatic" picks them (VS Code first, as before).
/// Paths are relative to the install roots searched below.
const PRODUCTS: &[(&str, &str, &[&str])] = &[
    ("vscode", "VS Code", &["Microsoft VS Code\\Code.exe"]),
    ("vscode-insiders", "VS Code Insiders", &["Microsoft VS Code Insiders\\Code - Insiders.exe"]),
    ("cursor", "Cursor", &["cursor\\Cursor.exe"]),
    ("windsurf", "Windsurf", &["windsurf\\Windsurf.exe"]),
    ("antigravity", "Antigravity", &["Antigravity\\Antigravity.exe"]),
    ("kiro", "Kiro", &["Kiro\\Kiro.exe"]),
    ("vscodium", "VSCodium", &["VSCodium\\VSCodium.exe"]),
    ("trae", "Trae", &["Trae\\Trae.exe"]),
    ("zed", "Zed", &["Zed\\zed.exe"]),
];

/// Where installers put their programs: per user first, then the two machine-wide roots.
fn roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for (var, sub) in [("LocalAppData", Some("Programs")), ("ProgramFiles", None), ("ProgramFiles(x86)", None)] {
        if let Some(base) = std::env::var_os(var) {
            let mut p = PathBuf::from(base);
            if let Some(s) = sub {
                p.push(s);
            }
            v.push(p);
        }
    }
    v
}

/// A string value from HKLM (either registry view: a 32-bit Notepad++ on 64-bit Windows
/// registers under WOW6432Node).
fn reg_string(key: &str, value: &str, wow64: u32) -> Option<String> {
    let (k, v) = (crate::wide::wide(key), crate::wide::wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | windows::Win32::System::Registry::REG_ROUTINE_FLAGS(wow64);
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            flags,
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

/// Every folder-opening editor found on this PC, in [`PRODUCTS`] order, with Notepad++ next
/// (it opens a folder as a workspace).
pub fn detect() -> Vec<Editor> {
    let roots = roots();
    let mut v = Vec::new();
    for (id, name, rels) in PRODUCTS {
        let exe = rels.iter().find_map(|rel| roots.iter().map(|r| r.join(rel)).find(|p| p.is_file()));
        if let Some(exe) = exe {
            v.push(Editor { id, name, exe });
        }
    }
    // Notepad++: its own registry key knows the install folder; the usual roots as a fallback.
    const WO64: u32 = 0x0100; // RRF_SUBKEY_WOW6464KEY
    const WO32: u32 = 0x0200; // RRF_SUBKEY_WOW6432KEY
    let npp = [WO64, WO32]
        .iter()
        .filter_map(|f| reg_string(r"SOFTWARE\Notepad++", "InstallPath", *f).map(PathBuf::from))
        .map(|dir| dir.join("notepad++.exe"))
        .chain(roots.iter().map(|r| r.join(r"Notepad++\notepad++.exe")))
        .find(|p| p.is_file());
    if let Some(exe) = npp {
        v.push(Editor { id: "notepadpp", name: "Notepad++", exe });
    }
    v
}

/// Windows' own Notepad (always present): "Edit in Notepad" on a single file.
pub fn notepad() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|r| Path::new(&r).join("System32\\notepad.exe"))
        .unwrap_or_else(|| PathBuf::from("notepad.exe"))
}

static CACHE: std::sync::Mutex<Option<Vec<Editor>>> = std::sync::Mutex::new(None);

/// Fills the cache in the background (called once at startup): the settings page then lists
/// the editors without touching the disk or the registry while rendering.
pub fn warm() {
    std::thread::Builder::new()
        .name("editor-detect".into())
        .spawn(|| {
            *CACHE.lock().unwrap() = Some(detect());
        })
        .ok();
}

/// The startup-warmed list (empty until [`warm`] finished). Never blocks.
pub fn cached() -> Vec<Editor> {
    CACHE.lock().unwrap().clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn notepad_is_where_windows_puts_it() {
        let p = super::notepad();
        assert!(p.is_file(), "{p:?}");
    }

    #[test]
    fn detection_runs_and_cache_warms() {
        // on the CI runner none of the editors is installed: an empty list is fine
        let found = super::detect();
        assert!(found.iter().all(|e| e.exe.is_file()));
        super::warm();
        for _ in 0..200 {
            if !super::cached().is_empty() || found.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(super::cached().len(), found.len());
    }
}
