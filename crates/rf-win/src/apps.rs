//! Installed apps, as Windows' "All apps" lists them (the AppsFolder: classic programs and Store
//! apps alike), for the quick search: name, an id to start it with, and its icon.
//!
//! Blocks (shell enumeration, a few hundred ms): call from a worker thread.

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

const APPS_FOLDER: &str = "shell:AppsFolder";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub name: String,
    /// The app's id in the AppsFolder (a Store app's AUMID, or a program's path / id).
    pub id: String,
}

impl App {
    /// What the shell takes to start it, or to draw its icon.
    pub fn parsing(&self) -> String {
        format!("{APPS_FOLDER}\\{}", self.id)
    }
}

fn display(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Entries of "All apps" that are documents or web links (a readme, a help file, a vendor's
/// web page), not something to start.
fn is_document(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    [".url", ".chm", ".pdf", ".txt", ".html", ".htm", ".rtf", ".doc", ".docx", ".hlp", ".ini", ".log"].iter().any(|e| l.ends_with(e))
        || l.starts_with("http://")
        || l.starts_with("https://")
}

/// An "Uninstall X" shortcut: nothing to start from a search.
fn is_uninstaller(name: &str) -> bool {
    let l = name.to_lowercase();
    l.starts_with("uninstall") || l.contains("kaldır") || l.ends_with(" uninstall") || l.ends_with(" uninstaller")
}

/// Everything in "All apps", sorted by name.
pub fn list() -> anyhow::Result<Vec<App>> {
    ensure_com();
    let w = wide(APPS_FOLDER);
    let folder: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)? };
    let en: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems)? };
    let mut apps = Vec::new();
    loop {
        let mut buf: [Option<IShellItem>; 64] = std::array::from_fn(|_| None);
        let mut got = 0u32;
        let _ = unsafe { en.Next(&mut buf, Some(&mut got)) };
        for item in buf.iter().take(got as usize).flatten() {
            let (Some(id), Some(name)) = (display(item, SIGDN_PARENTRELATIVEPARSING), display(item, SIGDN_NORMALDISPLAY)) else { continue };
            if !is_document(&id) && !is_uninstaller(&name) && !name.trim().is_empty() {
                apps.push(App { name, id });
            }
        }
        if got < 64 {
            break;
        }
    }
    // the same program from the all-users and the per-user Start menu: one row
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.name.to_lowercase() == b.name.to_lowercase());
    Ok(apps)
}

/// Starts `app`.
pub fn launch(app: &App) -> anyhow::Result<()> {
    crate::shellfolder::open(&app.parsing())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_not_apps() {
        assert!(is_document(r"C:\Program Files\X\readme.txt"));
        assert!(is_document("https://example.com"));
        assert!(!is_document("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert!(!is_document(r"{6D809377-6AF0-444B-8957-A3773F02200E}\Notepad++\notepad++.exe"));
        assert!(is_uninstaller("Uninstall Notepad++"));
        assert!(is_uninstaller("WinRAR'ı kaldır"));
        assert!(!is_uninstaller("Notepad++"));
    }

    /// Lists the apps of this PC (needs a desktop session; Explorer, Settings or Notepad is in
    /// every Windows install).
    #[test]
    fn lists_installed_apps() {
        let t = std::time::Instant::now();
        let apps = list().expect("apps folder");
        eprintln!("{} apps in {} ms", apps.len(), t.elapsed().as_millis());
        assert!(apps.len() > 5, "{apps:?}");
        assert!(apps.iter().all(|a| !a.id.is_empty() && !a.name.is_empty()));
    }
}

