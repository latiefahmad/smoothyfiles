//! Windows' common file dialogs (IFileOpenDialog / IFileSaveDialog) and the "start with
//! Windows" Run entry. Dialogs are modal to `owner`: call them on the shell STA thread.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog,
    IFileSaveDialog, SIGDN_FILESYSPATH,
};
use windows::core::{Interface, PCWSTR};

use crate::wide::wide;

pub enum Pick<'a> {
    /// Open an existing file; filters are (label, pattern) pairs like ("Ayar dosyası", "*.toml").
    File { filters: &'a [(&'a str, &'a str)] },
    /// Choose a folder.
    Folder,
    /// Save as `default_name`.
    Save { filters: &'a [(&'a str, &'a str)], default_name: &'a str },
}

/// Shows the dialog; `None` when cancelled.
pub fn pick(owner: HWND, title: &str, kind: Pick<'_>) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileDialog = match kind {
            Pick::Save { .. } => CoCreateInstance::<_, IFileSaveDialog>(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?.cast().ok()?,
            _ => CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?.cast().ok()?,
        };
        let mut opts = dialog.GetOptions().ok()? | FOS_FORCEFILESYSTEM;
        // keep the wide strings alive until Show returns
        let mut keep: Vec<Vec<u16>> = Vec::new();
        let mut specs: Vec<COMDLG_FILTERSPEC> = Vec::new();
        let filters: &[(&str, &str)] = match &kind {
            Pick::File { filters } | Pick::Save { filters, .. } => filters,
            Pick::Folder => &[],
        };
        for (label, pattern) in filters {
            keep.push(wide(label));
            let l = PCWSTR(keep.last().unwrap().as_ptr());
            keep.push(wide(pattern));
            let p = PCWSTR(keep.last().unwrap().as_ptr());
            specs.push(COMDLG_FILTERSPEC { pszName: l, pszSpec: p });
        }
        if !specs.is_empty() {
            let _ = dialog.SetFileTypes(&specs);
        }
        match &kind {
            Pick::Folder => opts |= FOS_PICKFOLDERS,
            Pick::Save { default_name, .. } => {
                opts |= FOS_OVERWRITEPROMPT;
                let n = wide(default_name);
                let _ = dialog.SetFileName(PCWSTR(n.as_ptr()));
            }
            Pick::File { .. } => {}
        }
        let _ = dialog.SetOptions(opts);
        let t = wide(title);
        let _ = dialog.SetTitle(PCWSTR(t.as_ptr()));
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "smoothyfiles";

/// Whether the app starts with Windows (HKCU Run entry, no admin rights needed).
pub fn starts_with_windows() -> bool {
    starts_with_windows_as(RUN_VALUE)
}

/// [`starts_with_windows`] for another program of the bundle (Run value `name`).
pub fn starts_with_windows_as(name: &str) -> bool {
    windows_registry::CURRENT_USER.open(RUN_KEY).and_then(|k| k.get_string(name)).is_ok()
}

pub fn set_start_with_windows(exe: &Path, on: bool) -> windows::core::Result<()> {
    set_start_with_windows_as(RUN_VALUE, exe, &[], on)
}

/// Run value `name` → `"exe" args…` (HKCU, no admin rights).
pub fn set_start_with_windows_as(name: &str, exe: &Path, args: &[&str], on: bool) -> windows::core::Result<()> {
    let key = windows_registry::CURRENT_USER.create(RUN_KEY)?;
    if on {
        let mut cmd = format!("\"{}\"", exe.display());
        for a in args {
            cmd.push(' ');
            cmd.push_str(a);
        }
        key.set_string(name, cmd)
    } else {
        match key.remove_value(name) {
            Ok(()) => Ok(()),
            // already absent
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
            Err(e) => Err(e),
        }
    }
}
