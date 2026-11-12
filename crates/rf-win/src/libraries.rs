//! Windows Libraries (Documents, Music, Pictures, Videos, user-created ones). A library is a
//! virtual merge of several folders; we resolve it to its file-system folders so the UI can show
//! the merged contents (Files only opens the default folder). Blocking: off the UI thread.

use std::path::PathBuf;

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::UI::Shell::{
    DSFT_DETECT, IShellItem, IShellItemArray, IShellLibrary, LFF_FORCEFILESYSTEM, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, ShellLibrary,
};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// The `.library-ms` file (stable identity).
    pub file: PathBuf,
    /// Localized display name ("Belgeler", "Music"…).
    pub name: String,
    /// File-system folders, in library order.
    pub folders: Vec<PathBuf>,
    /// Where new items go when the library is the target.
    pub default_save: Option<PathBuf>,
}

fn libraries_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Libraries"))
}

fn display(item: &IShellItem, sigdn: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Loads one library file.
pub fn load(file: &std::path::Path) -> Option<Library> {
    ensure_com();
    let w = wide(file);
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let lib: IShellLibrary = CoCreateInstance(&ShellLibrary, None, CLSCTX_INPROC_SERVER).ok()?;
        lib.LoadLibraryFromItem(&item, STGM_READ.0).ok()?;
        let name = display(&item, SIGDN_NORMALDISPLAY)
            .unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        let mut folders = Vec::new();
        if let Ok(arr) = lib.GetFolders::<IShellItemArray>(LFF_FORCEFILESYSTEM) {
            let n = arr.GetCount().unwrap_or(0);
            for i in 0..n {
                if let Ok(it) = arr.GetItemAt(i) {
                    if let Some(p) = display(&it, SIGDN_FILESYSPATH) {
                        folders.push(PathBuf::from(p));
                    }
                }
            }
        }
        let default_save = lib
            .GetDefaultSaveFolder::<IShellItem>(DSFT_DETECT)
            .ok()
            .and_then(|it| display(&it, SIGDN_FILESYSPATH))
            .map(PathBuf::from);
        Some(Library { file: file.to_path_buf(), name, folders, default_save })
    }
}

/// All libraries of the current user, sorted by name. Libraries with no file-system folder
/// (empty, or only non-FS locations) are skipped.
pub fn list() -> Vec<Library> {
    let Some(dir) = libraries_dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<Library> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("library-ms")))
        .filter_map(|p| load(&p))
        .filter(|l| !l.folders.is_empty())
        .collect();
    out.sort_by(|a, b| rf_core::sort::natural_cmp(&a.name, &b.name));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_libraries() {
        let libs = super::list();
        // Every listed library resolves to at least one existing folder.
        for l in &libs {
            assert!(!l.folders.is_empty(), "{l:?}");
            eprintln!("{} -> {:?} (save: {:?})", l.name, l.folders, l.default_save);
        }
    }
}
