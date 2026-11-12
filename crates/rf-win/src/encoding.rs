//! Legacy text encodings. Old Turkish text files are Windows-1254, not UTF-8; decoding them
//! as UTF-8 turns ş/ğ/ı into U+FFFD. Use the user locale's *traditional* ANSI code page (not
//! `GetACP()`, which is 65001 when "Use Unicode UTF-8 for worldwide language support" is on).

use std::sync::OnceLock;

use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_IDEFAULTANSICODEPAGE, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::core::PCWSTR;

pub fn legacy_ansi_codepage() -> u32 {
    static CP: OnceLock<u32> = OnceLock::new();
    *CP.get_or_init(|| {
        let mut buf = [0u16; 8];
        let n = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_IDEFAULTANSICODEPAGE, Some(&mut buf)) };
        let cp = if n > 1 { String::from_utf16_lossy(&buf[..n as usize - 1]).trim().parse().unwrap_or(1252) } else { 1252 };
        if cp == 0 || cp == 65001 { 1252 } else { cp }
    })
}

fn decode(raw: &[u8], cp: u32) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    let n = unsafe { MultiByteToWideChar(cp, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), raw, None) };
    if n <= 0 {
        return None;
    }
    let mut out = vec![0u16; n as usize];
    let n = unsafe { MultiByteToWideChar(cp, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), raw, Some(&mut out)) };
    (n > 0).then(|| String::from_utf16_lossy(&out[..n as usize]))
}

/// Text from bytes of unknown encoding: UTF-8 if valid (a truncated trailing sequence is
/// tolerated, since previews read only the head of a file), otherwise the legacy ANSI page.
pub fn decode_text(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        // valid up to a multi-byte char cut off by the read limit: still UTF-8
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(&raw[..e.valid_up_to()]).into_owned(),
        Err(_) => decode(raw, legacy_ansi_codepage()).unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_1254_and_utf8() {
        // "Şirket ığüşöç" in Windows-1254
        let raw = [0xDE, b'i', b'r', b'k', b'e', b't', b' ', 0xFD, 0xF0, 0xFC, 0xFE, 0xF6, 0xE7];
        assert_eq!(decode(&raw, 1254).as_deref(), Some("Şirket ığüşöç"));
        if legacy_ansi_codepage() == 1254 {
            assert_eq!(decode_text(&raw), "Şirket ığüşöç");
        }
        assert_eq!(decode_text("Şirket".as_bytes()), "Şirket");
        // a UTF-8 'ş' cut in half at the end stays UTF-8
        let mut cut = "abcş".as_bytes().to_vec();
        cut.pop();
        assert_eq!(decode_text(&cut), "abc");
    }
}

/// True when the user's regional format is Turkish (sorting then treats `I` as capital `ı`).
pub fn user_locale_is_turkish() -> bool {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(&mut buf) };
    n > 1 && String::from_utf16_lossy(&buf[..n as usize - 1]).to_ascii_lowercase().starts_with("tr")
}
