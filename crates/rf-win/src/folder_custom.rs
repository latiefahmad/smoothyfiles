//! A folder's own icon (desktop.ini), for the Properties window's Customize tab. Blocking disk work
//! (and a shell dialog for `pick_icon`): call off the UI thread.

use std::path::Path;

use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
use windows::Win32::UI::Shell::{PickIconDlg, SHCNE_UPDATEITEM, SHCNF_PATHW, SHChangeNotify};
use windows::core::PCWSTR;

use crate::wide::{from_wide, wide};

const SECTION: &str = ".ShellClassInfo";
const ATTR_READONLY: u32 = 0x1;
const ATTR_HIDDEN: u32 = 0x2;
const ATTR_SYSTEM: u32 = 0x4;
const ATTR_NORMAL: u32 = 0x80;

/// desktop.ini bytes → text: UTF-16 (either byte order) or UTF-8 with a BOM, else ANSI-ish (UTF-8 when it
/// is valid, otherwise one char per byte).
fn decode(b: &[u8]) -> String {
    let u16s = |b: &[u8], le: bool| -> String {
        let v: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&v)
    };
    if b.starts_with(&[0xff, 0xfe]) {
        u16s(&b[2..], true)
    } else if b.starts_with(&[0xfe, 0xff]) {
        u16s(&b[2..], false)
    } else if b.starts_with(&[0xef, 0xbb, 0xbf]) {
        String::from_utf8_lossy(&b[3..]).into_owned()
    } else {
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => b.iter().map(|&c| c as char).collect(),
        }
    }
}

/// What Explorer writes: UTF-16LE with a BOM.
fn encode(text: &str) -> Vec<u8> {
    let mut out = vec![0xff, 0xfe];
    for u in text.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

fn section_name(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']')
}

/// `key=value` of a line, key trimmed.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    Some((k.trim(), v.trim()))
}

fn get_key(lines: &[String], section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for l in lines {
        if let Some(s) = section_name(l) {
            inside = s.eq_ignore_ascii_case(section);
        } else if inside && let Some((k, v)) = key_value(l) && k.eq_ignore_ascii_case(key) {
            return Some(v.to_string());
        }
    }
    None
}

/// Removes every `key` in `section`.
fn remove_key(lines: &mut Vec<String>, section: &str, key: &str) {
    let mut inside = false;
    lines.retain(|l| {
        if let Some(s) = section_name(l) {
            inside = s.eq_ignore_ascii_case(section);
            true
        } else {
            !(inside && key_value(l).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)))
        }
    });
}

/// Puts `key=value` into `section` (replacing the key, creating the section at the top when absent).
fn set_key(lines: &mut Vec<String>, section: &str, key: &str, value: &str) {
    remove_key(lines, section, key);
    let entry = format!("{key}={value}");
    match lines.iter().position(|l| section_name(l).is_some_and(|s| s.eq_ignore_ascii_case(section))) {
        Some(i) => lines.insert(i + 1, entry),
        None => {
            lines.insert(0, entry);
            lines.insert(0, format!("[{section}]"));
        }
    }
}

/// Drops sections left without any key and blank lines; true when nothing meaningful remains.
fn tidy(lines: &mut Vec<String>) -> bool {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if section_name(&lines[i]).is_some() {
            let mut j = i + 1;
            while j < lines.len() && section_name(&lines[j]).is_none() {
                j += 1;
            }
            if lines[i + 1..j].iter().any(|l| !l.trim().is_empty()) {
                out.push(lines[i].clone());
                out.extend(lines[i + 1..j].iter().filter(|l| !l.trim().is_empty()).cloned());
            }
            i = j;
        } else {
            if !lines[i].trim().is_empty() {
                out.push(lines[i].clone());
            }
            i += 1;
        }
    }
    *lines = out;
    lines.is_empty()
}

fn read_lines(ini: &Path) -> Vec<String> {
    std::fs::read(ini).map(|b| decode(&b).lines().map(str::to_string).collect()).unwrap_or_default()
}

fn set_raw_attrs(path: &Path, attrs: u32) -> std::io::Result<()> {
    let w = wide(path);
    unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attrs)) }.map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

fn attrs_of(path: &Path) -> Option<u32> {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| m.file_attributes())
}

/// The folder's own icon from its desktop.ini (`[.ShellClassInfo]` `IconResource=path,index`, or `IconFile` +
/// `IconIndex`), as (file, index); None when it uses the default. The file is as stored (may hold %env%).
pub fn folder_icon(dir: &Path) -> Option<(String, i32)> {
    let lines = read_lines(&dir.join("desktop.ini"));
    if let Some(res) = get_key(&lines, SECTION, "IconResource").filter(|s| !s.is_empty()) {
        // "path,index"; the index may be negative (a resource id) and the path may itself hold commas
        return Some(match res.rsplit_once(',') {
            Some((f, i)) if i.trim().parse::<i32>().is_ok() => (f.trim().trim_matches('"').to_string(), i.trim().parse().unwrap_or(0)),
            _ => (res.trim_matches('"').to_string(), 0),
        });
    }
    let file = get_key(&lines, SECTION, "IconFile").filter(|s| !s.is_empty())?;
    let index = get_key(&lines, SECTION, "IconIndex").and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((file.trim_matches('"').to_string(), index))
}

/// Sets (Some) or removes (None) the folder's icon the way Explorer does: edits desktop.ini (UTF-16LE with a
/// BOM, other sections and keys kept), marks it hidden + system, and sets the folder's READONLY attribute
/// (what makes Explorer read desktop.ini). Removing drops the icon keys, deletes desktop.ini when nothing else
/// is left in it and then clears the folder's READONLY bit; either way the shell is told to refresh the folder.
pub fn set_folder_icon(dir: &Path, icon: Option<(&str, i32)>) -> anyhow::Result<()> {
    let ini = dir.join("desktop.ini");
    let mut lines = read_lines(&ini);
    let existed = ini.exists();
    match icon {
        Some((file, index)) => {
            remove_key(&mut lines, SECTION, "IconFile");
            remove_key(&mut lines, SECTION, "IconIndex");
            set_key(&mut lines, SECTION, "IconResource", &format!("{file},{index}"));
            tidy(&mut lines);
            if existed {
                // a hidden + system file cannot be overwritten until those bits are off
                set_raw_attrs(&ini, ATTR_NORMAL)?;
            }
            let mut text = lines.join("\r\n");
            text.push_str("\r\n");
            std::fs::write(&ini, encode(&text))?;
            set_raw_attrs(&ini, ATTR_HIDDEN | ATTR_SYSTEM)?;
            let cur = attrs_of(dir).unwrap_or(0);
            if cur & ATTR_READONLY == 0 {
                set_raw_attrs(dir, cur | ATTR_READONLY)?;
            }
        }
        None => {
            if !existed {
                return Ok(());
            }
            remove_key(&mut lines, SECTION, "IconResource");
            remove_key(&mut lines, SECTION, "IconFile");
            remove_key(&mut lines, SECTION, "IconIndex");
            let empty = tidy(&mut lines);
            set_raw_attrs(&ini, ATTR_NORMAL)?;
            if empty {
                std::fs::remove_file(&ini)?;
                let cur = attrs_of(dir).unwrap_or(0);
                if cur & ATTR_READONLY != 0 {
                    let a = cur & !ATTR_READONLY;
                    set_raw_attrs(dir, if a == 0 { ATTR_NORMAL } else { a })?;
                }
            } else {
                let mut text = lines.join("\r\n");
                text.push_str("\r\n");
                std::fs::write(&ini, encode(&text))?;
                set_raw_attrs(&ini, ATTR_HIDDEN | ATTR_SYSTEM)?;
            }
        }
    }
    let w = wide(dir);
    // SAFETY: a NUL-terminated wide path, read during the call
    unsafe { SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW, Some(w.as_ptr() as *const _), None) };
    Ok(())
}

/// Expands `%NAME%` references from the process environment (unknown names stay as written).
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        out.push_str(&rest[..a]);
        let after = &rest[a + 1..];
        match after.find('%') {
            Some(b) if b > 0 && std::env::var(&after[..b]).is_ok() => {
                out.push_str(&std::env::var(&after[..b]).unwrap_or_default());
                rest = &after[b + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Windows' "Change Icon" dialog (PickIconDlg from shell32) owned by `owner`, starting at `file`,`index`
/// (default shell32.dll, 3). Returns the chosen (file, index), environment variables expanded, or None when
/// cancelled. Must run on an STA thread with a message loop (callers use `crate::sta::run`).
pub fn pick_icon(owner: HWND, file: &str, index: i32) -> Option<(String, i32)> {
    crate::shell::ensure_com();
    let start = if file.is_empty() { r"%SystemRoot%\System32\shell32.dll".to_string() } else { file.to_string() };
    let start = expand_env(&start);
    let mut buf = [0u16; 520];
    let w = wide(&start);
    let n = w.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&w[..n]);
    let mut idx = if file.is_empty() { 3 } else { index };
    // SAFETY: buf is a writable, NUL-terminated buffer of the length passed; idx outlives the call
    let ok = unsafe { PickIconDlg(Some(owner), &mut buf, Some(&mut idx)) };
    if ok == 0 {
        return None;
    }
    let chosen = expand_env(&from_wide(&buf));
    if chosen.is_empty() { None } else { Some((chosen, idx)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rf-folder-custom-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn set_read_remove_icon() {
        let d = temp("a");
        assert_eq!(folder_icon(&d), None);
        set_folder_icon(&d, Some((r"C:\Windows\System32\shell32.dll", 4))).unwrap();
        assert_eq!(folder_icon(&d), Some((r"C:\Windows\System32\shell32.dll".to_string(), 4)));
        let ini = d.join("desktop.ini");
        let bytes = std::fs::read(&ini).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xfe]); // UTF-16LE BOM like Explorer's
        let a = attrs_of(&ini).unwrap();
        assert!(a & ATTR_HIDDEN != 0 && a & ATTR_SYSTEM != 0);
        assert!(attrs_of(&d).unwrap() & ATTR_READONLY != 0);
        // change it (the hidden + system file is overwritten)
        set_folder_icon(&d, Some((r"C:\x\y.ico", 0))).unwrap();
        assert_eq!(folder_icon(&d), Some((r"C:\x\y.ico".to_string(), 0)));
        set_folder_icon(&d, None).unwrap();
        assert_eq!(folder_icon(&d), None);
        assert!(!ini.exists());
        assert!(attrs_of(&d).unwrap() & ATTR_READONLY == 0);
        set_folder_icon(&d, None).unwrap(); // nothing to remove: fine
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unrelated_keys_survive_and_legacy_form_is_read() {
        let d = temp("b");
        let ini = d.join("desktop.ini");
        std::fs::write(&ini, "[.ShellClassInfo]\r\nIconFile=C:\\a.dll\r\nIconIndex=-7\r\nInfoTip=Merhaba\r\n\r\n[ViewState]\r\nMode=\r\nVid=\r\n").unwrap();
        assert_eq!(folder_icon(&d), Some((r"C:\a.dll".to_string(), -7)));
        set_folder_icon(&d, Some((r"C:\b.ico", 0))).unwrap();
        assert_eq!(folder_icon(&d), Some((r"C:\b.ico".to_string(), 0)));
        let text = decode(&std::fs::read(&ini).unwrap());
        assert!(text.contains("InfoTip=Merhaba") && text.contains("[ViewState]") && !text.contains("IconIndex"), "{text}");
        set_folder_icon(&d, None).unwrap();
        assert_eq!(folder_icon(&d), None);
        // other content remains: file and READONLY stay
        let text = decode(&std::fs::read(&ini).unwrap());
        assert!(text.contains("InfoTip=Merhaba") && !text.contains("IconResource"), "{text}");
        assert!(attrs_of(&d).unwrap() & ATTR_READONLY != 0);
        // clean up: hidden + system must come off before the tree can be removed
        set_raw_attrs(&ini, ATTR_NORMAL).unwrap();
        set_raw_attrs(&d, 0x10).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn env_expansion() {
        assert_eq!(expand_env("100%"), "100%");
        assert_eq!(expand_env("%NO_SUCH_VAR_X%\\a"), "%NO_SUCH_VAR_X%\\a");
        if let Ok(r) = std::env::var("SystemRoot") {
            assert_eq!(expand_env(r"%SystemRoot%\x"), format!(r"{r}\x"));
        }
    }
}
