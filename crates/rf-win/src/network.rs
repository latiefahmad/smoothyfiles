//! Network locations: shares of a server (`\\server` is not listable with FindFirstFile) and the
//! user's network shortcuts. Blocking and potentially slow (SMB timeouts): off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
use windows::Win32::Storage::FileSystem::{NetShareEnum, SHARE_INFO_1, STYPE_DISKTREE, STYPE_SPECIAL};
use windows::core::PCWSTR;

use crate::wide::wide;

const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
const ERROR_MORE_DATA: u32 = 234;

/// `\\server` or `\\server\` (no share part) → `Some("server")`.
pub fn server_root(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let rest = s.strip_prefix(r"\\")?;
    if rest.starts_with('?') || rest.starts_with('.') {
        return None;
    }
    let rest = rest.trim_end_matches('\\');
    (!rest.is_empty() && !rest.contains('\\')).then(|| rest.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    pub remark: String,
}

/// Disk shares of `server` (hidden `$` and admin shares excluded).
pub fn shares(server: &str) -> std::io::Result<Vec<Share>> {
    let w = wide(format!(r"\\{server}"));
    let mut out = Vec::new();
    let mut resume: u32 = 0;
    loop {
        let mut buf: *mut u8 = std::ptr::null_mut();
        let (mut read, mut total) = (0u32, 0u32);
        let rc = unsafe {
            NetShareEnum(PCWSTR(w.as_ptr()), 1, &mut buf, MAX_PREFERRED_LENGTH, &mut read, &mut total, Some(&mut resume))
        };
        if rc != 0 && rc != ERROR_MORE_DATA {
            if !buf.is_null() {
                unsafe { NetApiBufferFree(Some(buf as *const _)) };
            }
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        if !buf.is_null() {
            let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
            for it in items {
                if it.shi1_type.0 & !STYPE_SPECIAL.0 != STYPE_DISKTREE.0 || it.shi1_type.0 & STYPE_SPECIAL.0 != 0 {
                    continue;
                }
                let name = unsafe { it.shi1_netname.to_string() }.unwrap_or_default();
                if name.is_empty() || name.ends_with('$') {
                    continue;
                }
                let remark = if it.shi1_remark.is_null() { String::new() } else { unsafe { it.shi1_remark.to_string() }.unwrap_or_default() };
                out.push(Share { name, remark });
            }
            unsafe { NetApiBufferFree(Some(buf as *const _)) };
        }
        if rc != ERROR_MORE_DATA {
            break;
        }
    }
    out.sort_by(|a, b| rf_core::sort::natural_cmp(&a.name, &b.name));
    Ok(out)
}

/// Folders under `%APPDATA%\Microsoft\Windows\Network Shortcuts` ("Add a network location").
/// Each is a folder holding `target.lnk`; returns (display name, target).
pub fn network_shortcuts() -> Vec<(String, PathBuf)> {
    let Some(dir) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Network Shortcuts")) else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let lnk = p.join("target.lnk");
            let target = crate::shortcut::resolve(&lnk).ok()?.target;
            if target.as_os_str().is_empty() {
                return None;
            }
            Some((e.file_name().to_string_lossy().into_owned(), target))
        })
        .collect();
    out.sort_by(|a, b| rf_core::sort::natural_cmp(&a.0, &b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_root_parsing() {
        assert_eq!(server_root(Path::new(r"\\nas")), Some("nas".into()));
        assert_eq!(server_root(Path::new(r"\\nas\")), Some("nas".into()));
        assert_eq!(server_root(Path::new(r"\\nas\share")), None);
        assert_eq!(server_root(Path::new(r"\\?\C:\x")), None);
        assert_eq!(server_root(Path::new(r"C:\x")), None);
    }

    #[test]
    fn local_shares() {
        // localhost always answers (may legitimately be empty without user shares)
        let r = shares("localhost");
        assert!(r.is_ok(), "{r:?}");
        eprintln!("{:?}", r.unwrap());
        eprintln!("{:?}", network_shortcuts());
    }
}

/// Windows' own "Map network drive" dialog (drive letter, folder, reconnect at sign-in,
/// other credentials). Modal to `owner`; returns when it closes.
pub fn map_drive_dialog(owner: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::NetworkManagement::WNet::{RESOURCETYPE_DISK, WNetConnectionDialog};
    // SAFETY: plain Win32 dialog call on a valid (or null) owner window
    unsafe {
        let _ = WNetConnectionDialog(HWND(owner as *mut _), RESOURCETYPE_DISK.0);
    }
}

/// Windows' "Disconnect network drive" dialog.
pub fn disconnect_drive_dialog(owner: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::NetworkManagement::WNet::{RESOURCETYPE_DISK, WNetDisconnectDialog};
    // SAFETY: plain Win32 dialog call on a valid (or null) owner window
    unsafe {
        let _ = WNetDisconnectDialog(Some(HWND(owner as *mut _)), RESOURCETYPE_DISK.0);
    }
}
