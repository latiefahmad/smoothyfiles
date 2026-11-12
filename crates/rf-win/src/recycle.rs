//! Recycle Bin through the shell namespace (list / restore / empty / info).
//! Blocking + COM: call off the UI thread.

use windows::Win32::Foundation::{HWND, PROPERTYKEY, S_OK};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::System::SystemServices::SFGAO_FOLDER;
use windows::Win32::System::Variant::{PSTF_UTC, VARIANT, VariantToFileTime, VariantToStringAlloc, VariantToUInt64};
use windows::Win32::UI::Shell::{
    CMF_NORMAL, CMINVOKECOMMANDINFO, FOLDERID_RecycleBinFolder, IContextMenu, IEnumIDList, IShellFolder2,
    KF_FLAG_DEFAULT, SHBindToObject, SHCONTF_FOLDERS, SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS,
    SHEmptyRecycleBinW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHGDN_FORPARSING, SHGDN_NORMAL,
    SHGetKnownFolderIDList, SHQUERYRBINFO, SHQueryRecycleBinW, StrRetToBufW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, SW_SHOWNORMAL};
use windows::core::{GUID, PCSTR, PCWSTR, s};

use crate::shell::ensure_com;

/// One item in the Recycle Bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleItem {
    /// Original file name as shown by Explorer.
    pub name: String,
    /// Folder the item was deleted from.
    pub original_location: String,
    /// Deletion time, Unix milliseconds (UTC); 0 when unknown.
    pub deleted_ms: i64,
    pub size: u64,
    /// Real location inside `$Recycle.Bin\<SID>\$R...`; also the key used by [`restore`].
    pub parsing_path: String,
    pub is_dir: bool,
}

// {9B174B33-40FF-11D2-A27E-00C04FC30871}: pid 2 = original location, pid 3 = date deleted.
const FMTID_DISPLACED: GUID = GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871);
const PKEY_DELETED_FROM: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_DISPLACED, pid: 2 };
const PKEY_DATE_DELETED: PROPERTYKEY = PROPERTYKEY { fmtid: FMTID_DISPLACED, pid: 3 };
// System.Size
const PKEY_SIZE: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC), pid: 12 };

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) }
        }
    }
}

fn bin_folder() -> anyhow::Result<IShellFolder2> {
    ensure_com();
    unsafe {
        let pidl = Pidl(SHGetKnownFolderIDList(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT.0 as u32, None)?);
        Ok(SHBindToObject::<_, _, IShellFolder2>(None, pidl.0, None)?)
    }
}

/// Calls `f(folder, relative_pidl)` for each item in the bin.
fn for_each_item(folder: &IShellFolder2, mut f: impl FnMut(&IShellFolder2, *mut ITEMIDLIST)) -> anyhow::Result<()> {
    unsafe {
        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
        let hr = folder.EnumObjects(HWND::default(), flags, &mut en);
        if hr != S_OK {
            hr.ok()?; // S_FALSE with no enumerator = empty bin
            return Ok(());
        }
        let Some(en) = en else { return Ok(()) };
        loop {
            let mut item = [std::ptr::null_mut::<ITEMIDLIST>()];
            let mut fetched = 0u32;
            if en.Next(&mut item, Some(&mut fetched)) != S_OK || fetched == 0 {
                break;
            }
            let p = Pidl(item[0]);
            f(folder, p.0);
        }
    }
    Ok(())
}

fn display_name(folder: &IShellFolder2, pidl: *mut ITEMIDLIST, flags: windows::Win32::UI::Shell::SHGDNF) -> String {
    unsafe {
        let mut sr = STRRET::default();
        if folder.GetDisplayNameOf(pidl as *const _, flags, &mut sr).is_err() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        if StrRetToBufW(&mut sr, Some(pidl as *const _), &mut buf).is_ok() {
            crate::wide::from_wide(&buf)
        } else {
            String::new()
        }
    }
}

fn detail(folder: &IShellFolder2, pidl: *mut ITEMIDLIST, key: &PROPERTYKEY) -> Option<VARIANT> {
    unsafe { folder.GetDetailsEx(pidl as *const _, key).ok() }
}

fn read_item(folder: &IShellFolder2, pidl: *mut ITEMIDLIST) -> RecycleItem {
    // The bin reports the full original path as the "normal" name; keep only the file name.
    let full = display_name(folder, pidl, SHGDN_NORMAL);
    let name = full.rsplit('\\').next().unwrap_or(&full).to_string();
    let parsing_path = display_name(folder, pidl, SHGDN_FORPARSING);
    let original_location = detail(folder, pidl, &PKEY_DELETED_FROM)
        .and_then(|v| unsafe { VariantToStringAlloc(&v).ok() })
        .map(|p| unsafe {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        })
        .unwrap_or_default();
    let deleted_ms = detail(folder, pidl, &PKEY_DATE_DELETED)
        .and_then(|v| unsafe { VariantToFileTime(&v, PSTF_UTC).ok() })
        .map(|ft| rf_core::format::filetime_to_unix_ms(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64))
        .unwrap_or(0);
    let size = detail(folder, pidl, &PKEY_SIZE).and_then(|v| unsafe { VariantToUInt64(&v).ok() }).unwrap_or(0);
    let mut attrs: u32 = SFGAO_FOLDER.0;
    let is_dir = unsafe { folder.GetAttributesOf(&[pidl as *const _], &mut attrs) }.is_ok() && attrs & SFGAO_FOLDER.0 != 0;
    RecycleItem { name, original_location, deleted_ms, size, parsing_path, is_dir }
}

/// Lists everything in the Recycle Bin (all drives), in shell order.
pub fn list() -> anyhow::Result<Vec<RecycleItem>> {
    let folder = bin_folder()?;
    let mut out = Vec::new();
    for_each_item(&folder, |f, p| out.push(read_item(f, p)))?;
    Ok(out)
}

/// Restores `items` to their original locations by invoking the shell's "undelete" verb
/// (name-collision prompts come from the shell). Items no longer in the bin are ignored;
/// errors if none matched.
pub fn restore(items: &[RecycleItem]) -> anyhow::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let folder = bin_folder()?;
    let wanted: Vec<String> = items.iter().map(|i| i.parsing_path.to_lowercase()).collect();
    // Keep the matching pidls alive past enumeration.
    let mut found: Vec<Pidl> = Vec::new();
    unsafe {
        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
        let hr = folder.EnumObjects(HWND::default(), flags, &mut en);
        if let (S_OK, Some(en)) = (hr, en) {
            loop {
                let mut item = [std::ptr::null_mut::<ITEMIDLIST>()];
                let mut fetched = 0u32;
                if en.Next(&mut item, Some(&mut fetched)) != S_OK || fetched == 0 {
                    break;
                }
                let p = Pidl(item[0]);
                if wanted.contains(&display_name(&folder, p.0, SHGDN_FORPARSING).to_lowercase()) {
                    found.push(p);
                }
            }
        }
        anyhow::ensure!(!found.is_empty(), "Öğeler Geri Dönüşüm Kutusu'nda bulunamadı");
        let raw: Vec<*const ITEMIDLIST> = found.iter().map(|p| p.0 as *const _).collect();
        let cm: IContextMenu = folder.GetUIObjectOf(HWND::default(), &raw, None)?;
        let menu = CreatePopupMenu()?;
        let res = (|| -> anyhow::Result<()> {
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                lpVerb: PCSTR(s!("undelete").as_ptr()),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            cm.InvokeCommand(&info)?;
            Ok(())
        })();
        let _ = DestroyMenu(menu);
        res
    }
}

/// Empties the bin on all drives. With `confirm == false` no confirmation dialog, progress UI or
/// sound is shown (the caller is expected to have asked already).
pub fn empty(confirm: bool) -> anyhow::Result<()> {
    ensure_com();
    let flags = if confirm { 0 } else { SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND };
    unsafe { SHEmptyRecycleBinW(None, PCWSTR::null(), flags)? };
    Ok(())
}

/// `(item_count, total_bytes)` across all drives.
pub fn info() -> anyhow::Result<(u64, u64)> {
    let mut i = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut i)? };
    Ok((i.i64NumItems.max(0) as u64, i.i64Size.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_works() {
        let (count, bytes) = info().unwrap();
        println!("recycle bin: {count} items, {bytes} bytes");
    }

    #[test]
    fn list_is_ok_and_consistent() {
        let items = list().unwrap();
        for i in &items {
            assert!(!i.parsing_path.is_empty() && !i.name.contains('\\'));
        }
        println!("{items:#?}");
    }

    #[test]
    fn restore_empty_slice_is_noop() {
        restore(&[]).unwrap();
    }
}
