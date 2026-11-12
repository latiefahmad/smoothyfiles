//! Localized folder names, the way Explorer shows them: a read-only or system folder whose
//! `desktop.ini` has `LocalizedResourceName` (C:\Users → "Kullanıcılar", Downloads →
//! "İndirilenler" on a Turkish system). Only the shown name changes; paths stay real.
//!
//! Results are cached per folder and per resource string (a handful of distinct ones), so
//! breadcrumbs and tab titles can ask on every frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Storage::FileSystem::GetFileAttributesW;
use windows::Win32::UI::Shell::SHLoadIndirectString;
use windows::core::PCWSTR;

use crate::wide::{from_wide, long_path, wide};

const READONLY: u32 = 0x1;
const SYSTEM: u32 = 0x4;
const DIRECTORY: u32 = 0x10;

type Cache<K> = Mutex<HashMap<K, Option<Arc<str>>>>;

fn by_path() -> &'static Cache<PathBuf> {
    static C: OnceLock<Cache<PathBuf>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn by_resource() -> &'static Cache<String> {
    static C: OnceLock<Cache<String>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The localized name of folder `dir`, if it has one. `attrs` are the folder's attributes
/// when already known (from a listing), else they are read.
pub fn folder_label(dir: &Path, attrs: Option<u32>) -> Option<Arc<str>> {
    if let Some(hit) = by_path().lock().ok()?.get(dir) {
        return hit.clone();
    }
    // breadcrumbs and tab titles ask while drawing: never touch a (possibly offline) network
    // path from here (the drive-type check itself does no network I/O)
    if is_network(dir) {
        return None;
    }
    let a = attrs.unwrap_or_else(|| unsafe { GetFileAttributesW(PCWSTR(long_path(dir).as_ptr())) });
    // Explorer only looks at desktop.ini of read-only or system folders (PathIsSystemFolder)
    let label = if a != u32::MAX && a & DIRECTORY != 0 && a & (READONLY | SYSTEM) != 0 {
        std::fs::read(dir.join("desktop.ini")).ok().and_then(|raw| resource_name(&decode(&raw))).and_then(|r| resolve(&r))
    } else {
        None
    };
    if let Ok(mut c) = by_path().lock() {
        if c.len() > 4096 {
            c.clear();
        }
        c.insert(dir.to_path_buf(), label.clone());
    }
    label
}

/// A UNC path or a network drive letter (reading desktop.ini there costs a round trip).
pub fn is_network(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    if s.starts_with(r"\\?\UNC\") || (s.starts_with(r"\\") && !s.starts_with(r"\\?\")) {
        return true;
    }
    let letter = match b {
        [l, b':', ..] => *l,
        [b'\\', b'\\', b'?', b'\\', l, b':', ..] => *l,
        _ => return false,
    };
    let root = wide(format!("{}:\\", letter as char));
    // DRIVE_REMOTE = 4
    unsafe { windows::Win32::Storage::FileSystem::GetDriveTypeW(PCWSTR(root.as_ptr())) == 4 }
}

/// desktop.ini is UTF-16LE with a BOM, or the ANSI code page (ASCII in practice).
fn decode(raw: &[u8]) -> String {
    if raw.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = raw[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw)).into_owned()
    }
}

/// `LocalizedResourceName=` in the `[.ShellClassInfo]` section.
fn resource_name(ini: &str) -> Option<String> {
    let mut in_section = false;
    for line in ini.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[.ShellClassInfo]");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("LocalizedResourceName") {
                let v = v.trim();
                return (!v.is_empty()).then(|| v.to_string());
            }
        }
    }
    None
}

/// "@%SystemRoot%\system32\shell32.dll,-21813" → the string in the user's UI language; a
/// plain value is used as is.
fn resolve(res: &str) -> Option<Arc<str>> {
    if !res.starts_with('@') {
        return Some(res.into());
    }
    if let Some(hit) = by_resource().lock().ok()?.get(res) {
        return hit.clone();
    }
    let got = unsafe {
        let input = wide(expand_env(res));
        let mut out = vec![0u16; 512];
        SHLoadIndirectString(PCWSTR(input.as_ptr()), &mut out, None).ok().map(|_| from_wide(&out)).filter(|s| !s.is_empty()).map(Arc::<str>::from)
    };
    if let Ok(mut c) = by_resource().lock() {
        c.insert(res.to_string(), got.clone());
    }
    got
}

/// `%Name%` → the environment variable's value (unknown names stay as written).
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ini() {
        let ini = "\r\n[.ShellClassInfo]\r\nLocalizedResourceName=@%SystemRoot%\\system32\\shell32.dll,-21813\r\n";
        assert_eq!(resource_name(ini).as_deref(), Some("@%SystemRoot%\\system32\\shell32.dll,-21813"));
        assert_eq!(resource_name("[Other]\nLocalizedResourceName=x\n"), None);
        assert_eq!(resource_name("[.shellclassinfo]\nIconResource=a\nlocalizedresourcename = Klasörüm\n").as_deref(), Some("Klasörüm"));
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("[.ShellClassInfo]\nLocalizedResourceName=Ad\n".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(resource_name(&decode(&utf16)).as_deref(), Some("Ad"));
    }

    #[test]
    fn users_folder_has_a_name() {
        // C:\Users ships with LocalizedResourceName on every Windows install
        let users = PathBuf::from(std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\Users");
        let got = folder_label(&users, None);
        assert!(got.as_deref().is_some_and(|s| !s.is_empty() && !s.starts_with('@')), "{got:?}");
        // a plain folder has none
        assert_eq!(folder_label(&std::env::temp_dir(), None), None);
    }

    #[test]
    fn network_paths() {
        assert!(is_network(Path::new(r"\\nas\paylaşım\klasör")));
        assert!(is_network(Path::new(r"\\?\UNC\nas\x")));
        assert!(!is_network(Path::new(r"C:\Users")));
        assert!(!is_network(Path::new(r"\\?\C:\Users")));
    }

    #[test]
    fn own_folder_with_plain_name() {
        let d = std::env::temp_dir().join(format!("rf-loc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("desktop.ini"), "[.ShellClassInfo]\r\nLocalizedResourceName=Özel Ad\r\n").unwrap();
        // not read-only / system yet: Explorer ignores the file, so do we
        assert_eq!(folder_label(&d, Some(DIRECTORY)), None);
        let sub = d.join("salt");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("desktop.ini"), "[.ShellClassInfo]\r\nLocalizedResourceName=Özel Ad\r\n").unwrap();
        assert_eq!(folder_label(&sub, Some(DIRECTORY | READONLY)).as_deref(), Some("Özel Ad"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
