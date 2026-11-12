//! Programs and libraries (EXE, DLL, SYS…): architecture from the PE header, plus the version
//! resource's file version, product name, company and description. Read-only.

use std::io::Read;
use std::path::Path;

use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerLanguageNameW, VerQueryValueW};
use windows::core::{HSTRING, PCWSTR};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PeInfo {
    /// "x64", "x86", "ARM64"…
    pub arch: &'static str,
    pub version: Option<String>,
    pub product: Option<String>,
    pub company: Option<String>,
    pub description: Option<String>,
    /// ProductVersion string ("10.0.26100.1" or whatever the vendor wrote).
    pub product_version: Option<String>,
    /// LegalCopyright.
    pub copyright: Option<String>,
    /// OriginalFilename: the name the file was built under.
    pub original_filename: Option<String>,
    /// The version resource's language, spelled out by Windows ("English (United States)").
    pub language: Option<String>,
    /// LegalTrademarks.
    pub trademarks: Option<String>,
    /// InternalName.
    pub internal_name: Option<String>,
}

fn machine_name(m: u16) -> &'static str {
    match m {
        0x8664 => "x64",
        0x014c => "x86",
        0xaa64 => "ARM64",
        0x01c4 => "ARM",
        0xa641 => "ARM64EC",
        0x0200 => "IA-64",
        _ => "?",
    }
}

/// The PE machine field, or None when the file isn't a PE image.
fn machine(head: &[u8]) -> Option<u16> {
    if head.len() < 0x40 || &head[..2] != b"MZ" {
        return None;
    }
    let off = u32::from_le_bytes(head[0x3c..0x40].try_into().ok()?) as usize;
    let sig = head.get(off..off + 6)?;
    if &sig[..4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([sig[4], sig[5]]))
}

pub fn info(path: &Path) -> Option<PeInfo> {
    let mut head = vec![0u8; 4096];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let arch = machine_name(machine(&head[..n])?);
    let mut out = PeInfo { arch, ..Default::default() };
    version_strings(path, &mut out);
    Some(out)
}

fn version_strings(path: &Path, out: &mut PeInfo) {
    let name = HSTRING::from(path.as_os_str());
    unsafe {
        let size = GetFileVersionInfoSizeW(&name, None);
        if size == 0 {
            return;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(&name, None, size, buf.as_mut_ptr().cast()).is_err() {
            return;
        }
        let query = |sub: &str| -> Option<(*const u8, u32)> {
            let q = HSTRING::from(sub);
            let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            VerQueryValueW(buf.as_ptr().cast(), PCWSTR(q.as_ptr()), &mut p, &mut len).as_bool().then_some((p as *const u8, len))
        };
        // fixed part: the numeric file version
        if let Some((p, len)) = query("\\") {
            if len as usize >= 52 {
                let f = std::slice::from_raw_parts(p, 52);
                let ms = u32::from_le_bytes(f[8..12].try_into().unwrap());
                let ls = u32::from_le_bytes(f[12..16].try_into().unwrap());
                out.version = Some(format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff));
            }
        }
        // strings: the first language/code page the file lists, else US English Unicode
        let translation = query("\\VarFileInfo\\Translation").filter(|(_, len)| *len >= 4).map(|(p, _)| {
            let t = std::slice::from_raw_parts(p, 4);
            (u16::from_le_bytes([t[0], t[1]]), u16::from_le_bytes([t[2], t[3]]))
        });
        let lang = translation.map(|(l, cp)| format!("{l:04x}{cp:04x}")).unwrap_or_else(|| "040904b0".into());
        // the language id spelled out by Windows, e.g. "English (United States)"
        if let Some((l, _)) = translation {
            let mut name = [0u16; 128];
            let n = VerLanguageNameW(l as u32, &mut name) as usize;
            if n > 0 {
                out.language = Some(String::from_utf16_lossy(&name[..n.min(name.len())]).trim_end_matches('\0').to_string()).filter(|s| !s.is_empty());
            }
        }
        let text = |key: &str| -> Option<String> {
            let (p, len) = query(&format!("\\StringFileInfo\\{lang}\\{key}"))?;
            let w = std::slice::from_raw_parts(p as *const u16, len as usize);
            let s = String::from_utf16_lossy(w).trim_end_matches('\0').trim().to_string();
            (!s.is_empty()).then_some(s)
        };
        out.product = text("ProductName");
        out.company = text("CompanyName");
        out.description = text("FileDescription");
        out.product_version = text("ProductVersion");
        out.copyright = text("LegalCopyright");
        out.original_filename = text("OriginalFilename");
        out.trademarks = text("LegalTrademarks");
        out.internal_name = text("InternalName");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_notepad() {
        let p = std::path::Path::new(r"C:\Windows\System32\notepad.exe");
        if !p.exists() {
            return;
        }
        let i = super::info(p).expect("notepad is a PE image");
        assert_ne!(i.arch, "?");
        assert!(i.version.is_some());
        assert!(i.company.as_deref().unwrap_or("").contains("Microsoft"), "{i:?}");
    }

    #[test]
    fn reads_extra_version_fields() {
        let p = std::path::Path::new(r"C:\Windows\System32\cmd.exe");
        if !p.exists() {
            return;
        }
        let i = super::info(p).expect("cmd is a PE image");
        assert!(i.copyright.as_deref().unwrap_or("").contains("Microsoft"), "{i:?}");
        assert!(i.language.is_some(), "{i:?}");
        assert!(i.product_version.is_some(), "{i:?}");
        assert!(i.original_filename.is_some(), "{i:?}");
    }

    #[test]
    fn non_pe_is_none() {
        assert_eq!(super::machine(b"hello"), None);
        let mut h = vec![0u8; 0x100];
        h[..2].copy_from_slice(b"MZ");
        h[0x3c] = 0x80;
        h[0x80..0x86].copy_from_slice(b"PE\0\0\x64\x86");
        assert_eq!(super::machine(&h), Some(0x8664));
    }
}
