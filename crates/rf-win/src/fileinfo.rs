//! Facts for the Properties window: times, attributes, size on disk, default app. Blocking
//! (disk / registry): call off the UI thread.

use std::os::windows::fs::MetadataExt;
use std::path::Path;

use windows::Win32::Storage::FileSystem::{
    CreateFileW, DecryptFileW, EncryptFileW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetCompressedFileSizeW, GetDiskFreeSpaceW, GetVolumeInformationW, GetVolumePathNameW,
    INVALID_FILE_SIZE, OPEN_EXISTING, SetFileAttributesW,
};
use windows::Win32::UI::Shell::{ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_FRIENDLYAPPNAME, AssocQueryStringW};
use windows::core::{PCWSTR, PWSTR, w};

use crate::wide::{from_wide, wide};

#[derive(Debug, Clone, Default)]
pub struct Info {
    pub is_dir: bool,
    pub size: u64,
    /// Allocated size (compression / sparse / cluster rounding). Files only.
    pub size_on_disk: Option<u64>,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub accessed_ms: i64,
    pub attrs: u32,
    /// Friendly name of the app that opens this type ("Fotoğraflar"). Files only.
    pub opens_with: Option<String>,
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn info(path: &Path) -> std::io::Result<Info> {
    let m = std::fs::metadata(path)?;
    let is_dir = m.is_dir();
    Ok(Info {
        is_dir,
        size: if is_dir { 0 } else { m.len() },
        size_on_disk: if is_dir { None } else { size_on_disk(path) },
        created_ms: ms(m.created()),
        modified_ms: ms(m.modified()),
        accessed_ms: ms(m.accessed()),
        attrs: m.file_attributes(),
        opens_with: if is_dir { None } else { opens_with(path) },
    })
}

/// Bytes the file occupies: compressed size rounded up to whole clusters.
pub fn size_on_disk(path: &Path) -> Option<u64> {
    let w = wide(path);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(w.as_ptr()), Some(&mut high)) };
    if low == INVALID_FILE_SIZE && unsafe { windows::Win32::Foundation::GetLastError() }.0 != 0 {
        return None;
    }
    let raw = ((high as u64) << 32) | low as u64;
    let cluster = cluster_size(path).unwrap_or(4096).max(1);
    Some(raw.div_ceil(cluster) * cluster)
}

fn cluster_size(path: &Path) -> Option<u64> {
    let root = path.ancestors().last()?.to_path_buf();
    let mut r = root.to_string_lossy().to_string();
    if !r.ends_with('\\') {
        r.push('\\');
    }
    let w = wide(r);
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    unsafe { GetDiskFreeSpaceW(PCWSTR(w.as_ptr()), Some(&mut spc), Some(&mut bps), Some(&mut free), Some(&mut total)) }.ok()?;
    Some(spc as u64 * bps as u64)
}

/// Friendly name of the default app for the file's type.
pub fn opens_with(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_string();
    let ext = wide(format!(".{ext}"));
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    unsafe {
        AssocQueryStringW(ASSOCF_INIT_IGNOREUNKNOWN, ASSOCSTR_FRIENDLYAPPNAME, PCWSTR(ext.as_ptr()), w!("open"), Some(PWSTR(buf.as_mut_ptr())), &mut len)
            .ok()
            .ok()?;
    }
    Some(from_wide(&buf)).filter(|s| !s.is_empty())
}

fn zone_stream(path: &Path) -> std::ffi::OsString {
    let mut s = path.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    s
}

/// Whether Windows treats the file as downloaded (its Zone.Identifier stream names the Internet
/// or Restricted zone): Explorer's "This file came from another computer… Unblock".
pub fn zone_blocked(path: &Path) -> bool {
    std::fs::read(zone_stream(path)).is_ok_and(|b| {
        // usually ANSI, sometimes UTF-16 (written by other tools)
        let text = if b.starts_with(&[0xff, 0xfe]) {
            String::from_utf16_lossy(&b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
        } else {
            String::from_utf8_lossy(&b).into_owned()
        };
        text.lines().any(|l| l.trim().strip_prefix("ZoneId=").and_then(|v| v.trim().parse::<u32>().ok()).is_some_and(|z| z >= 3))
    })
}

/// Removes the downloaded mark (the Zone.Identifier stream), as Explorer's "Unblock" does.
pub fn unblock(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(zone_stream(path)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Sets / clears read-only and hidden, keeping the other attribute bits.
pub fn set_flags(path: &Path, readonly: Option<bool>, hidden: Option<bool>) -> std::io::Result<()> {
    let cur = std::fs::metadata(path)?.file_attributes();
    let mut a = cur;
    let mut put = |bit: u32, on: Option<bool>| match on {
        Some(true) => a |= bit,
        Some(false) => a &= !bit,
        None => {}
    };
    put(FILE_ATTRIBUTE_READONLY.0, readonly);
    put(FILE_ATTRIBUTE_HIDDEN.0, hidden);
    if a == cur {
        return Ok(());
    }
    let w = wide(path);
    unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(a)) }.map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

pub const ARCHIVE: u32 = 0x20;
pub const NOT_INDEXED: u32 = 0x2000;
pub const COMPRESSED: u32 = 0x800;
pub const ENCRYPTED: u32 = 0x4000;
const READONLY: u32 = 0x1;
const HIDDEN: u32 = 0x2;
const REPARSE_POINT: u32 = 0x400;
/// The bits `set_attrs` is allowed to change.
const SETTABLE: u32 = READONLY | HIDDEN | ARCHIVE | NOT_INDEXED;

fn os_err(e: windows::core::Error) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// Sets the bits in `set` and clears those in `clear` among READONLY/HIDDEN/ARCHIVE/NOT_CONTENT_INDEXED
/// (others ignored), keeping the rest. Acts on a link itself, not its target.
pub fn set_attrs(path: &Path, set: u32, clear: u32) -> std::io::Result<()> {
    set_attrs_changed(path, set, clear).map(|_| ())
}

/// Like `set_attrs`; true when the attributes actually changed.
fn set_attrs_changed(path: &Path, set: u32, clear: u32) -> std::io::Result<bool> {
    let cur = std::fs::symlink_metadata(path)?.file_attributes();
    let a = (cur & !(clear & SETTABLE)) | (set & SETTABLE);
    if a == cur {
        return Ok(false);
    }
    // SetFileAttributes refuses 0: "no attributes" is FILE_ATTRIBUTE_NORMAL
    let a = if a == 0 { 0x80 } else { a };
    let w = wide(path);
    unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(a)) }.map_err(os_err)?;
    Ok(true)
}

/// `set_attrs` on the folder and everything under it (reparse points / junctions are changed themselves but
/// not entered). Returns how many items changed; the first error stops. `cancel` is polled per item and ends
/// the walk early with what was done so far.
pub fn set_attrs_tree(dir: &Path, set: u32, clear: u32, cancel: &dyn Fn() -> bool) -> std::io::Result<usize> {
    let mut changed = 0usize;
    if cancel() {
        return Ok(0);
    }
    if set_attrs_changed(dir, set, clear)? {
        changed += 1;
    }
    if std::fs::symlink_metadata(dir)?.file_attributes() & REPARSE_POINT != 0 {
        return Ok(changed);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            if cancel() {
                return Ok(changed);
            }
            let p = e?.path();
            if set_attrs_changed(&p, set, clear)? {
                changed += 1;
            }
            let m = std::fs::symlink_metadata(&p)?;
            if m.is_dir() && m.file_attributes() & REPARSE_POINT == 0 {
                stack.push(p);
            }
        }
    }
    Ok(changed)
}

/// NTFS compression on or off for a file or folder (FSCTL_SET_COMPRESSION, COMPRESSION_FORMAT_DEFAULT / NONE;
/// a folder only changes what new files get).
pub fn set_compressed(path: &Path, on: bool) -> std::io::Result<()> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            0xC000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS, // lets a folder be opened
            None,
        )
    }
    .map_err(os_err)?;
    let fmt: u16 = if on { 1 } else { 0 }; // COMPRESSION_FORMAT_DEFAULT / COMPRESSION_FORMAT_NONE
    let mut ret = 0u32;
    let r = unsafe { DeviceIoControl(h, FSCTL_SET_COMPRESSION, Some(&fmt as *const u16 as *const _), 2, None, 0, Some(&mut ret), None) };
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
    r.map_err(os_err)
}

/// EFS encryption on or off (EncryptFileW / DecryptFileW).
pub fn set_encrypted(path: &Path, on: bool) -> std::io::Result<()> {
    let w = wide(path);
    if on {
        unsafe { EncryptFileW(PCWSTR(w.as_ptr())) }.map_err(os_err)
    } else {
        unsafe { DecryptFileW(PCWSTR(w.as_ptr()), None) }.map_err(os_err)
    }
}

/// What the volume holding `path` supports: (compression, encryption), from the FILE_FILE_COMPRESSION /
/// FILE_SUPPORTS_ENCRYPTION flags of GetVolumeInformationW.
pub fn volume_caps(path: &Path) -> (bool, bool) {
    let w = wide(path);
    let mut root = [0u16; 261];
    if unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut root) }.is_err() {
        return (false, false);
    }
    let mut flags = 0u32;
    if unsafe { GetVolumeInformationW(PCWSTR(root.as_ptr()), None, None, None, Some(&mut flags), None) }.is_err() {
        return (false, false);
    }
    (flags & 0x10 != 0, flags & 0x2_0000 != 0)
}

/// The path a folder is shared as on the network (`\\PC\Paylaşım`), from the LanmanServer share list
/// (REG_MULTI_SZ values holding `Path=...`). Case-insensitive; admin shares (ending in `$`) are ignored.
/// None when not shared or the list is not readable.
pub fn share_of(dir: &Path) -> Option<String> {
    let key = windows_registry::LOCAL_MACHINE.open(r"SYSTEM\CurrentControlSet\Services\LanmanServer\Shares").ok()?;
    let want = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    for (name, _) in key.values().ok()? {
        if name.ends_with('$') {
            continue;
        }
        let Ok(lines) = key.get_multi_string(&name) else { continue };
        let path = lines.iter().find_map(|l| l.strip_prefix("Path=")).map(|p| p.trim_end_matches(['\\', '/']).to_lowercase());
        if path.as_deref() == Some(want.as_str()) {
            let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
            return Some(format!(r"\\{pc}\{name}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn archive_and_not_indexed_toggle() {
        let dir = std::env::temp_dir().join(format!("rf-attrs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        super::set_attrs(&f, super::NOT_INDEXED, super::ARCHIVE).unwrap();
        let a = super::info(&f).unwrap().attrs;
        assert!(a & super::NOT_INDEXED != 0 && a & super::ARCHIVE == 0);
        super::set_attrs(&f, super::ARCHIVE, super::NOT_INDEXED).unwrap();
        let a = super::info(&f).unwrap().attrs;
        assert!(a & super::NOT_INDEXED == 0 && a & super::ARCHIVE != 0);
        // bits outside the four are ignored
        super::set_attrs(&f, super::COMPRESSED | super::ENCRYPTED, 0).unwrap();
        assert!(super::info(&f).unwrap().attrs & (super::COMPRESSED | super::ENCRYPTED) == 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compression_toggles_on_ntfs() {
        let dir = std::env::temp_dir().join(format!("rf-compress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, vec![b'a'; 100_000]).unwrap();
        if super::volume_caps(&f).0 {
            super::set_compressed(&f, true).unwrap();
            assert!(super::info(&f).unwrap().attrs & super::COMPRESSED != 0);
            super::set_compressed(&f, false).unwrap();
            assert!(super::info(&f).unwrap().attrs & super::COMPRESSED == 0);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attrs_tree_and_share_lookup() {
        use super::{NOT_INDEXED, info, set_attrs_tree};
        let dir = std::env::temp_dir().join(format!("rf-attrs-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("alt/derin")).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("alt/b.txt"), b"x").unwrap();
        std::fs::write(dir.join("alt/derin/c.txt"), b"x").unwrap();
        // 3 folders (the root, alt, derin) + 3 files
        assert_eq!(set_attrs_tree(&dir, NOT_INDEXED, 0, &|| false).unwrap(), 6);
        assert!(info(&dir.join("alt/derin/c.txt")).unwrap().attrs & NOT_INDEXED != 0);
        assert!(info(&dir).unwrap().attrs & NOT_INDEXED != 0);
        assert_eq!(set_attrs_tree(&dir, NOT_INDEXED, 0, &|| false).unwrap(), 0);
        assert_eq!(set_attrs_tree(&dir, 0, NOT_INDEXED, &|| true).unwrap(), 0); // cancelled at once
        assert_eq!(set_attrs_tree(&dir, 0, NOT_INDEXED, &|| false).unwrap(), 6);
        assert_eq!(super::share_of(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn downloaded_mark_is_read_and_removed() {
        let p = std::env::temp_dir().join(format!("rf-zone-{}.exe", std::process::id()));
        std::fs::write(&p, b"MZ").unwrap();
        assert!(!super::zone_blocked(&p));
        std::fs::write(super::zone_stream(&p), "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/\r\n").unwrap();
        assert!(super::zone_blocked(&p));
        super::unblock(&p).unwrap();
        assert!(!super::zone_blocked(&p));
        assert_eq!(std::fs::read(&p).unwrap(), b"MZ"); // the file itself untouched
        super::unblock(&p).unwrap(); // nothing to remove: fine
        // a local-intranet mark is not a block
        std::fs::write(super::zone_stream(&p), "[ZoneTransfer]\r\nZoneId=1\r\n").unwrap();
        assert!(!super::zone_blocked(&p));
        std::fs::remove_file(&p).unwrap();
    }

    use super::*;

    #[test]
    fn info_and_flags_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rf-fileinfo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, vec![b'x'; 10]).unwrap();
        let i = info(&f).unwrap();
        assert_eq!(i.size, 10);
        assert!(i.size_on_disk.unwrap() >= 10 || i.size_on_disk == Some(0));
        set_flags(&f, Some(true), Some(true)).unwrap();
        let a = info(&f).unwrap().attrs;
        assert!(a & FILE_ATTRIBUTE_READONLY.0 != 0 && a & FILE_ATTRIBUTE_HIDDEN.0 != 0);
        set_flags(&f, Some(false), Some(false)).unwrap();
        let a = info(&f).unwrap().attrs;
        assert!(a & FILE_ATTRIBUTE_READONLY.0 == 0 && a & FILE_ATTRIBUTE_HIDDEN.0 == 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
