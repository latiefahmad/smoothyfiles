//! Volume details and safe removal. Blocking (disks can stall): call off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{CMF_NORMAL, CMINVOKECOMMANDINFO, IContextMenu, IShellFolder, SHBindToParent, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, SW_SHOWNORMAL};
use windows::core::{PCSTR, PCWSTR, s};

use crate::shell::ensure_com;
use crate::wide::{from_wide, wide};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub root: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub serial: u32,
    pub total: u64,
    pub free: u64,
    /// Turkish, e.g. "Yerel Disk".
    pub drive_type: String,
}

impl VolumeInfo {
    /// "A1B2-C3D4", as shown by `vol`.
    pub fn serial_string(&self) -> String {
        format!("{:04X}-{:04X}", self.serial >> 16, self.serial & 0xFFFF)
    }
}

/// Turkish name for a `GetDriveTypeW` result.
pub fn drive_type_name(t: u32) -> &'static str {
    match t {
        2 => "Çıkarılabilir Disk",
        3 => "Yerel Disk",
        4 => "Ağ Sürücüsü",
        5 => "Optik Sürücü (CD/DVD)",
        6 => "RAM Disk",
        1 => "Bağlı Değil",
        _ => "Bilinmiyor",
    }
}

/// Details of the volume at `root` (e.g. `C:\`). Size fields stay 0 when the media is missing.
pub fn volume_info(root: &Path) -> anyhow::Result<VolumeInfo> {
    let mut r = root.to_string_lossy().to_string();
    if !r.ends_with('\\') {
        r.push('\\');
    }
    let w = wide(&r);
    let drive_type = drive_type_name(unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) }).to_string();
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    let has_vol = unsafe {
        GetVolumeInformationW(PCWSTR(w.as_ptr()), Some(&mut label), Some(&mut serial), None, None, Some(&mut fs))
    };
    let (mut free, mut total) = (0u64, 0u64);
    if has_vol.is_ok() {
        unsafe {
            let _ = GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None);
        }
    } else if drive_type != "Optik Sürücü (CD/DVD)" {
        has_vol?; // media-less optical drives are reported with empty details instead of failing
    }
    Ok(VolumeInfo {
        root: PathBuf::from(r),
        label: from_wide(&label),
        filesystem: from_wide(&fs),
        serial,
        total,
        free,
        drive_type,
    })
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) }
        }
    }
}

/// Ejects a removable (or optical) drive by invoking the shell's "Eject" verb, which is exactly
/// what Explorer does (flushes, locks, dismounts and asks the device to power down).
/// Fails when files are still open on the drive.
pub fn eject(drive_letter: char) -> anyhow::Result<()> {
    let letter = drive_letter.to_ascii_uppercase();
    anyhow::ensure!(letter.is_ascii_uppercase(), "Geçersiz sürücü harfi");
    let root = format!("{letter}:\\");
    let rw = wide(&root);
    let t = unsafe { GetDriveTypeW(PCWSTR(rw.as_ptr())) };
    anyhow::ensure!(t == 2 || t == 5, "{letter}: sürücüsü çıkarılabilir değil");

    ensure_com();
    unsafe {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR(rw.as_ptr()), None, &mut pidl, 0, None)?;
        let pidl = Pidl(pidl);
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let parent: IShellFolder = SHBindToParent(pidl.0, Some(&mut child))?;
        let cm: IContextMenu = parent.GetUIObjectOf(HWND::default(), &[child as *const ITEMIDLIST], None)?;
        let menu = CreatePopupMenu()?;
        let res = (|| -> anyhow::Result<()> {
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                lpVerb: PCSTR(s!("eject").as_ptr()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_drive_info() {
        let v = volume_info(Path::new("C:\\")).unwrap();
        assert_eq!(v.drive_type, "Yerel Disk");
        assert!(v.total > 0 && v.free <= v.total);
        assert!(!v.filesystem.is_empty());
        assert_eq!(v.serial_string().len(), 9);
    }

    #[test]
    fn eject_rejects_fixed_drive() {
        // Safe: C: is a fixed disk, so this must fail before touching the shell.
        assert!(eject('C').is_err());
    }
}
