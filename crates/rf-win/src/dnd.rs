//! Drag files out of smoothyfiles (to Explorer, the desktop, other apps — or back into
//! smoothyfiles itself) with the standard shell data object and drag image.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{ILFindLastID, SHCreateDataObject, SHDoDragDrop, SHParseDisplayName};
use windows::core::PCWSTR;

use crate::fileops::Transfer;
use crate::wide::wide;

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) }
        }
    }
}

fn parse(path: &Path) -> anyhow::Result<Pidl> {
    let w = wide(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)? };
    Ok(Pidl(pidl))
}

/// Shell data object for items that share one parent folder.
pub fn data_object(items: &[PathBuf]) -> anyhow::Result<IDataObject> {
    anyhow::ensure!(!items.is_empty(), "no items");
    let parent = items[0].parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
    let folder = parse(parent)?;
    // Same-folder items only (child pidls are relative to `folder`).
    let items: Vec<&PathBuf> = items.iter().filter(|p| p.parent() == Some(parent)).collect();
    let abs: Vec<Pidl> = items.iter().map(|p| parse(p)).collect::<anyhow::Result<_>>()?;
    let children: Vec<*const ITEMIDLIST> = abs.iter().map(|p| unsafe { ILFindLastID(p.0) as *const _ }).collect();
    Ok(unsafe { SHCreateDataObject(Some(folder.0), Some(&children), None)? })
}

/// Runs a modal OLE drag on the calling thread (must be the shell STA thread — see `sta::run`).
/// `ui_thread` is the id of the thread that owns the window where the drag started; its input
/// state is attached so this thread can take mouse capture while the button is still down.
pub fn drag_out(owner: HWND, ui_thread: u32, items: &[PathBuf]) -> anyhow::Result<DROPEFFECT> {
    let data = data_object(items)?;
    unsafe {
        // The button may already be up (very short drags) — don't start a drag then.
        let down = |vk: i32| (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
        if !down(VK_LBUTTON.0 as i32) && !down(VK_RBUTTON.0 as i32) {
            return Ok(DROPEFFECT(0));
        }
        let me = GetCurrentThreadId();
        let attached = ui_thread != 0 && AttachThreadInput(me, ui_thread, true).as_bool();
        let r = SHDoDragDrop(Some(owner), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
        if attached {
            let _ = AttachThreadInput(me, ui_thread, false);
        }
        Ok(r?)
    }
}

/// Explorer's default for a plain drag: same volume → move, otherwise copy.
/// Ctrl forces copy, Shift forces move.
pub fn default_transfer(sources: &[PathBuf], dest_dir: &Path, ctrl: bool, shift: bool) -> Transfer {
    if ctrl {
        return Transfer::Copy;
    }
    if shift {
        return Transfer::Move;
    }
    let vol = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    match sources.first() {
        Some(s) if vol(s) == vol(dest_dir) => Transfer::Move,
        _ => Transfer::Copy,
    }
}

/// True when dropping `sources` into `dest_dir` makes no sense (onto itself or its own child).
pub fn is_invalid_drop(sources: &[PathBuf], dest_dir: &Path) -> bool {
    sources.iter().any(|s| dest_dir == s || dest_dir.starts_with(s)) || sources.iter().all(|s| s.parent() == Some(dest_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_rules() {
        let src = vec![PathBuf::from(r"C:\a\x.txt")];
        assert_eq!(default_transfer(&src, Path::new(r"C:\b"), false, false), Transfer::Move);
        assert_eq!(default_transfer(&src, Path::new(r"D:\b"), false, false), Transfer::Copy);
        assert_eq!(default_transfer(&src, Path::new(r"D:\b"), false, true), Transfer::Move);
        assert_eq!(default_transfer(&src, Path::new(r"C:\b"), true, false), Transfer::Copy);
        assert!(is_invalid_drop(&src, Path::new(r"C:\a")));
        assert!(is_invalid_drop(&[PathBuf::from(r"C:\a")], Path::new(r"C:\a\sub")));
        assert!(!is_invalid_drop(&src, Path::new(r"C:\b")));
    }

    #[test]
    fn builds_data_object() {
        let d = std::env::temp_dir().join(format!("rf-dnd-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        crate::shell::ensure_com();
        assert!(data_object(&[d.join("a.txt")]).is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
