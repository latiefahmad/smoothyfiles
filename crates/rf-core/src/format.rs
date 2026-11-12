use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use chrono::{Local, TimeZone};

static ENGLISH: AtomicBool = AtomicBool::new(false);
static DECIMAL: AtomicBool = AtomicBool::new(false);
static DATE_STYLE: AtomicU8 = AtomicU8::new(0);

/// How dates are shown (Settings → General → Date format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateStyle {
    /// Recent times as "5 dakika önce", older ones as a short date.
    Relative,
    /// "05.11.2026 13:45"
    Local,
    /// ISO 8601 "2026-11-05 13:45"
    Universal,
}

pub fn set_date_style(s: DateStyle) {
    DATE_STYLE.store(s as u8, Ordering::Relaxed);
}

fn date_style() -> DateStyle {
    match DATE_STYLE.load(Ordering::Relaxed) {
        0 => DateStyle::Relative,
        1 => DateStyle::Local,
        _ => DateStyle::Universal,
    }
}

/// Sizes in powers of 1000 instead of 1024 (Settings → Files & folders → Size format).
pub fn set_decimal_sizes(on: bool) {
    DECIMAL.store(on, Ordering::Relaxed);
}

/// UI language switch for the few words in number formats: English uses "bytes" and a decimal
/// point; the default (Turkish) uses "bayt" and a decimal comma.
pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

/// "0 bayt", "512 bayt", "1,2 KB", "34,5 MB", "1,02 GB" (Turkish decimal comma).
pub fn size(bytes: u64) -> String {
    size_with(bytes, ENGLISH.load(Ordering::Relaxed), DECIMAL.load(Ordering::Relaxed))
}

fn size_with(bytes: u64, en: bool, decimal: bool) -> String {
    let base: f64 = if decimal { 1000.0 } else { 1024.0 };
    let units: [&str; 6] = [if en { "bytes" } else { "bayt" }, "KB", "MB", "GB", "TB", "PB"];
    if (bytes as f64) < base {
        return format!("{bytes} {}", units[0]);
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= base && u < units.len() - 1 {
        v /= base;
        u += 1;
    }
    let s = if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    let s = if en { s } else { s.replace('.', ",") };
    format!("{s} {}", units[u])
}

/// A file time in the chosen [`DateStyle`].
pub fn datetime(ms: i64) -> String {
    let now = chrono::Local::now().timestamp_millis();
    datetime_with(ms, date_style(), now, ENGLISH.load(Ordering::Relaxed))
}

/// Always the full local date-time "05.11.2026 03:51" (details, tooltips).
pub fn datetime_full(ms: i64) -> String {
    datetime_with(ms, DateStyle::Local, 0, false)
}

fn datetime_with(ms: i64, style: DateStyle, now: i64, en: bool) -> String {
    if ms <= 0 {
        return String::new();
    }
    let chrono::LocalResult::Single(dt) = Local.timestamp_millis_opt(ms) else { return String::new() };
    match style {
        DateStyle::Universal => dt.format("%Y-%m-%d %H:%M").to_string(),
        DateStyle::Local => dt.format("%d.%m.%Y %H:%M").to_string(),
        DateStyle::Relative => {
            let secs = (now - ms) / 1000;
            let (n, tr, enu) = match secs {
                s if s < 0 => return dt.format("%d.%m.%Y %H:%M").to_string(),
                s if s < 45 => return (if en { "just now" } else { "az önce" }).to_string(),
                s if s < 3600 => (((s + 30) / 60).max(1), "dakika önce", "min ago"),
                s if s < 86_400 => (s / 3600, "saat önce", "h ago"),
                s if s < 7 * 86_400 => (s / 86_400, "gün önce", "d ago"),
                _ => return dt.format("%d.%m.%Y %H:%M").to_string(),
            };
            if en { format!("{n} {enu}") } else { format!("{n} {tr}") }
        }
    }
}

/// Windows FILETIME (100ns since 1601) → Unix ms.
pub fn filetime_to_unix_ms(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    (ft / 10_000) as i64 - 11_644_473_600_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 bayt");
        assert_eq!(size(1023), "1023 bayt");
        assert_eq!(size(1024), "1,00 KB");
        assert_eq!(size(1536), "1,50 KB");
        assert_eq!(size(10 * 1024 * 1024 + 1), "10,0 MB");
        assert_eq!(size(500 * 1024 * 1024 * 1024), "500 GB");
    }

    #[test]
    fn relative_and_formats() {
        let now = 1_800_000_000_000i64;
        assert_eq!(datetime_with(now - 10_000, DateStyle::Relative, now, false), "az önce");
        assert_eq!(datetime_with(now - 5 * 60_000, DateStyle::Relative, now, false), "5 dakika önce");
        assert_eq!(datetime_with(now - 3 * 3_600_000, DateStyle::Relative, now, true), "3 h ago");
        assert_eq!(datetime_with(now - 2 * 86_400_000, DateStyle::Relative, now, false), "2 gün önce");
        assert!(datetime_with(now - 30 * 86_400_000, DateStyle::Relative, now, false).contains('.'));
        assert_eq!(datetime_with(now, DateStyle::Universal, now, false).len(), 16);
        assert_eq!(size_with(1500, false, true), "1,50 KB");
        assert_eq!(size_with(1500, false, false), "1,46 KB");
    }

    #[test]
    fn filetime() {
        // 2000-01-01T00:00:00Z
        assert_eq!(filetime_to_unix_ms(125_911_584_000_000_000), 946_684_800_000);
        assert_eq!(filetime_to_unix_ms(0), 0);
    }
}
