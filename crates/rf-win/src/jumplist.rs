//! Taskbar jump list (right-click on the taskbar button): "New window" / "New tab" tasks and
//! the pinned folders, like Files. Blocking + COM: call off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, ShellLink,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::core::{Interface, PCWSTR};

use crate::shell::ensure_com;
use crate::wide::wide;

/// Command-line switch of the "New window" task.
pub const NEW_WINDOW_ARG: &str = "--new-window";

fn link(exe: &Path, args: &str, title: &str, icon: Option<(&str, i32)>) -> windows::core::Result<IShellLinkW> {
    unsafe {
        let l: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let we = wide(exe);
        l.SetPath(PCWSTR(we.as_ptr()))?;
        let wa = wide(args);
        l.SetArguments(PCWSTR(wa.as_ptr()))?;
        match icon {
            Some((file, ix)) => {
                let wi = wide(file);
                l.SetIconLocation(PCWSTR(wi.as_ptr()), ix)?;
            }
            None => l.SetIconLocation(PCWSTR(we.as_ptr()), 0)?,
        }
        let store: IPropertyStore = l.cast()?;
        store.SetValue(&PKEY_Title, &PROPVARIANT::from(title))?;
        store.Commit()?;
        Ok(l)
    }
}

/// Replaces the jump list: tasks + a "pinned" category (`(title, folder)`).
pub fn set(exe: &Path, tasks: &[(&str, &str)], pinned_title: &str, pinned: &[(String, PathBuf)]) -> anyhow::Result<()> {
    ensure_com();
    unsafe {
        let list: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        let mut slots = 0u32;
        let _removed: IObjectArray = list.BeginList(&mut slots)?;
        let folder_icon = std::env::var("SystemRoot").map(|r| format!(r"{r}\System32\imageres.dll")).unwrap_or_default();
        if !pinned.is_empty() {
            let coll: IObjectCollection = CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
            for (title, path) in pinned.iter().take(slots.max(5) as usize) {
                let args = format!("\"{}\"", path.display());
                if let Ok(l) = link(exe, &args, title, Some((&folder_icon, 3))) {
                    coll.AddObject(&l)?;
                }
            }
            let wt = wide(pinned_title);
            list.AppendCategory(PCWSTR(wt.as_ptr()), &coll.cast::<IObjectArray>()?)?;
        }
        if !tasks.is_empty() {
            let coll: IObjectCollection = CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
            for (title, args) in tasks {
                coll.AddObject(&link(exe, args, title, None)?)?;
            }
            list.AddUserTasks(&coll.cast::<IObjectArray>()?)?;
        }
        list.CommitList()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Writes a jump list for the *test binary's* own taskbar identity (not the app's).
    #[test]
    fn builds_jump_list() {
        let exe = std::env::current_exe().unwrap();
        let pins = vec![("Belgeler".to_string(), std::env::temp_dir())];
        super::set(&exe, &[("Yeni pencere", super::NEW_WINDOW_ARG)], "Sabitlenenler", &pins).unwrap();
    }
}