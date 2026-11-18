//! The user's Windows date and time formats, from the locale (Settings → Date format ›
//! System): "M/d/yyyy" and "h:mm:ss tt" become "11/17/2026 9:30 AM" style dates.

use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SSHORTDATE, LOCALE_STIMEFORMAT};
use windows::core::PCWSTR;

/// The Windows short date and time as chrono formats: the list one without seconds, the
/// details one with. `None` when the locale does not say.
pub fn short_datetime() -> Option<(String, String)> {
    let date = to_chrono(&locale_str(LOCALE_SSHORTDATE)?);
    let time = to_chrono(&locale_str(LOCALE_STIMEFORMAT)?);
    // the lists leave the seconds out, like Explorer's file lists
    let full = format!("{date} {time}");
    let list = full.replace(":%-S", "").replace(":%S", "");
    Some((list, full))
}

fn locale_str(lc_type: u32) -> Option<String> {
    let mut buf = [0u16; 80];
    // a null name is the user's default locale; n includes the terminating zero
    let n = unsafe { GetLocaleInfoEx(PCWSTR::null(), lc_type, Some(&mut buf)) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..(n - 1) as usize]))
}

/// Windows tokens to chrono ones: single letters are unpadded, 3–4 letter month and day names
/// become their strftime forms; separators and anything unknown pass through as plain text.
fn to_chrono(w: &str) -> String {
    let cs: Vec<char> = w.chars().collect();
    let mut out = String::with_capacity(w.len() * 2);
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let mut n = 1;
        while i + n < cs.len() && cs[i + n] == c {
            n += 1;
        }
        out.push_str(&token(c, n));
        i += n;
    }
    out
}

fn token(c: char, n: usize) -> String {
    match (c, n) {
        ('d', 1) => "%-d".into(),
        ('d', 2) => "%d".into(),
        ('d', 3) => "%a".into(),
        ('d', _) => "%A".into(),
        ('M', 1) => "%-m".into(),
        ('M', 2) => "%m".into(),
        ('M', 3) => "%b".into(),
        ('M', _) => "%B".into(),
        ('y', 1 | 2) => "%y".into(),
        ('y', _) => "%Y".into(),
        ('h', 1) => "%-I".into(),
        ('h', _) => "%I".into(),
        ('H', 1) => "%-H".into(),
        ('H', _) => "%H".into(),
        ('m', 1) => "%-M".into(),
        ('m', _) => "%M".into(),
        ('s', 1) => "%-S".into(),
        ('s', _) => "%S".into(),
        // AM/PM marker (a single "t" would only take the first letter: kept whole)
        ('t', _) => "%p".into(),
        // era cycles ("gg") and quoted literals: left out
        ('g', _) | ('\'', _) | ('"', _) => String::new(),
        (x, _) => x.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_tokens_become_chrono_ones() {
        assert_eq!(to_chrono("M/d/yyyy"), "%-m/%-d/%Y");
        assert_eq!(to_chrono("h:mm:ss tt"), "%-I:%M:%S %p");
        assert_eq!(to_chrono("dd.MM.yyyy HH:mm"), "%d.%m.%Y %H:%M");
        assert_eq!(to_chrono("yyyy'年'M月d日"), "%Y年%-m月%-d日");
    }
}
