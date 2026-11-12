use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// NUL-terminated UTF-16.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// UTF-16 buffer (possibly NUL-terminated) → String.
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Adds the `\\?\` prefix so paths longer than MAX_PATH work. With that prefix Windows
/// takes the path literally, so `/` separators are turned into backslashes first.
pub fn long_path(p: &Path) -> Vec<u16> {
    let s = p.as_os_str();
    let mut raw: Vec<u16> = s.encode_wide().collect();
    for c in raw.iter_mut() {
        if *c == u16::from(b'/') {
            *c = u16::from(b'\\');
        }
    }
    let has_prefix = raw.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]);
    let is_unc = raw.starts_with(&[b'\\' as u16, b'\\' as u16]);
    let mut out: Vec<u16> = Vec::with_capacity(raw.len() + 9);
    if !has_prefix {
        if is_unc {
            out.extend(r"\\?\UNC".encode_utf16());
            out.extend_from_slice(&raw[1..]); // keep one leading backslash
        } else {
            out.extend(r"\\?\".encode_utf16());
            out.extend_from_slice(&raw);
        }
    } else {
        out.extend_from_slice(&raw);
    }
    out.push(0);
    out
}
