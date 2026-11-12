//! UI language. The Turkish source string is the key; English comes from `i18n_en::EN`, the other
//! languages from `locales/<code>.json` (embedded at build time, see build.rs).
//!
//! `t!("Yeni klasör")` returns the string for the active language. Everything is looked up at
//! render time (nothing is cached at startup), so switching the language only needs a window
//! refresh. Strings produced on worker threads (e.g. date-group labels in a finished listing)
//! are translated again where they are drawn, via [`td`].
//!
//! A locale file maps ids to text: the id of a key is its FNV-1a 32 hash in hex
//! (`locales/_source.json` lists every key with its id; `scripts/i18n_check.py` checks files).
//! A missing text falls back to English, then to the Turkish source.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{LazyLock, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

include!(concat!(env!("OUT_DIR"), "/locales.rs"));

/// The language setting. `Other(i)` is `LOCALES[i]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    /// Follow the Windows UI language.
    #[default]
    System,
    Tr,
    En,
    Other(u16),
}

impl Lang {
    /// "system", "tr", "en" (kept as they were stored before), or a locale code ("de-DE").
    pub fn code(self) -> &'static str {
        match self {
            Lang::System => "system",
            Lang::Tr => "tr",
            Lang::En => "en",
            Lang::Other(i) => LOCALES.get(i as usize).map_or("en", |l| l.0),
        }
    }

    pub fn from_code(code: &str) -> Lang {
        match code {
            "system" => Lang::System,
            "tr" | "tr-TR" => Lang::Tr,
            "en" | "en-US" => Lang::En,
            c => LOCALES.iter().position(|l| l.0.eq_ignore_ascii_case(c)).map_or(Lang::System, |i| Lang::Other(i as u16)),
        }
    }

    /// `System` resolved to a concrete language.
    pub fn resolve(self) -> Lang {
        match self {
            Lang::System => system_lang(),
            other => other,
        }
    }

    /// The language's own name for itself ("Deutsch", "日本語").
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::System => "",
            Lang::Tr => "Türkçe",
            Lang::En => "English",
            Lang::Other(_) => {
                let code = self.code();
                NATIVE.iter().find(|n| n.0 == code).map_or(code, |n| n.1)
            }
        }
    }

    /// Every language offered: Turkish, English, then the locale files, by name. Right-to-left
    /// languages are left out until GPUI lays text out right to left (they draw reversed now).
    pub fn all() -> Vec<Lang> {
        let mut v = vec![Lang::Tr, Lang::En];
        let mut others: Vec<Lang> = (0..LOCALES.len() as u16).map(Lang::Other).filter(|l| !l.is_rtl()).collect();
        others.sort_by_key(|l| l.native_name().to_lowercase());
        v.extend(others);
        v
    }
}

impl Lang {
    fn is_rtl(self) -> bool {
        RTL.contains(&self.code())
    }
}

/// Written right to left; translated, but not offered yet (see [`Lang::all`]).
const RTL: &[&str] = &["ar", "he-IL", "fa-IR"];

impl Serialize for Lang {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Lang {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Lang::from_code(&String::deserialize(d)?))
    }
}

/// Names in their own language, for every locale Files ships (shown when a file exists).
const NATIVE: &[(&str, &str)] = &[
    ("af", "Afrikaans"),
    ("ar", "العربية"),
    ("be-BY", "Беларуская"),
    ("bg", "Български"),
    ("ca", "Català"),
    ("cs-CZ", "Čeština"),
    ("da", "Dansk"),
    ("de-DE", "Deutsch"),
    ("el", "Ελληνικά"),
    ("en-GB", "English (United Kingdom)"),
    ("es-419", "Español (Latinoamérica)"),
    ("es-ES", "Español (España)"),
    ("fa-IR", "فارسی"),
    ("fi-FI", "Suomi"),
    ("fil-PH", "Filipino"),
    ("fr-FR", "Français"),
    ("he-IL", "עברית"),
    ("hi-IN", "हिन्दी"),
    ("hr-HR", "Hrvatski"),
    ("hu-HU", "Magyar"),
    ("hy-AM", "Հայերեն"),
    ("id-ID", "Bahasa Indonesia"),
    ("it-IT", "Italiano"),
    ("ja-JP", "日本語"),
    ("ka", "ქართული"),
    ("km-KH", "ខ្មែរ"),
    ("ko-KR", "한국어"),
    ("lt-LT", "Lietuvių"),
    ("lv-LV", "Latviešu"),
    ("ms-MY", "Bahasa Melayu"),
    ("nb-NO", "Norsk bokmål"),
    ("nl-NL", "Nederlands"),
    ("pl-PL", "Polski"),
    ("pt-BR", "Português (Brasil)"),
    ("pt-PT", "Português (Portugal)"),
    ("ro-RO", "Română"),
    ("ru-RU", "Русский"),
    ("sk-SK", "Slovenčina"),
    ("sq-AL", "Shqip"),
    ("sr-Cyrl", "Српски"),
    ("sv-SE", "Svenska"),
    ("ta", "தமிழ்"),
    ("th-TH", "ไทย"),
    ("uk-UA", "Українська"),
    ("vi", "Tiếng Việt"),
    ("zh-Hans", "简体中文"),
    ("zh-Hant", "繁體中文"),
];

/// Picks the translation for a Windows language tag ("de-AT", "zh-TW", "sr-Cyrl-RS").
fn match_tag(tag: &str) -> Lang {
    let tag = tag.trim();
    let lower = tag.to_ascii_lowercase();
    let lang = lower.split('-').next().unwrap_or("");
    match lang {
        "tr" => return Lang::Tr,
        "en" if lower != "en-gb" => return Lang::En,
        // right to left: English until GPUI can draw it
        "ar" | "he" | "iw" | "fa" => return Lang::En,
        _ => {}
    }
    let pick = |code: &str| LOCALES.iter().position(|l| l.0.eq_ignore_ascii_case(code)).map(|i| Lang::Other(i as u16));
    // exact tag first ("pt-BR", "en-GB", "es-ES")
    if let Some(l) = pick(tag) {
        return l;
    }
    let special = match lang {
        "zh" => Some(if ["-tw", "-hk", "-mo", "-hant"].iter().any(|s| lower.contains(s)) { "zh-Hant" } else { "zh-Hans" }),
        "sr" => Some("sr-Cyrl"),
        "no" | "nb" | "nn" => Some("nb-NO"),
        "es" => Some(if lower == "es-es" { "es-ES" } else { "es-419" }),
        "pt" => Some(if lower == "pt-br" { "pt-BR" } else { "pt-PT" }),
        "en" => Some("en-GB"),
        _ => None,
    };
    if let Some(l) = special.and_then(pick) {
        return l;
    }
    // same language, any region ("de-AT" → "de-DE")
    LOCALES
        .iter()
        .position(|l| l.0.split('-').next().is_some_and(|x| x.eq_ignore_ascii_case(lang)))
        .map_or(Lang::En, |i| Lang::Other(i as u16))
}

/// The Windows UI language's best translation (English when there is none).
fn system_lang() -> Lang {
    use windows::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};
    let mut buf = [0u16; 85];
    let n = unsafe { LCIDToLocaleName(GetUserDefaultUILanguage() as u32, Some(&mut buf), 0) };
    if n <= 1 {
        return Lang::En;
    }
    match_tag(&String::from_utf16_lossy(&buf[..n as usize - 1]))
}

// 0 = Tr, 1 = En, 2 + i = LOCALES[i] (resolved; never System).
static ACTIVE: AtomicU16 = AtomicU16::new(0);

/// Sets the UI language (resolving `System`) and the number-format words in rf-core.
pub fn set(lang: Lang) {
    let l = lang.resolve();
    ACTIVE.store(
        match l {
            Lang::Tr | Lang::System => 0,
            Lang::En => 1,
            Lang::Other(i) => 2 + i,
        },
        Ordering::Relaxed,
    );
    rf_core::format::set_english(l != Lang::Tr);
}

/// The active (resolved) language.
pub fn active() -> Lang {
    match ACTIVE.load(Ordering::Relaxed) {
        0 => Lang::Tr,
        1 => Lang::En,
        n => Lang::Other(n - 2),
    }
}

pub fn is_tr() -> bool {
    active() == Lang::Tr
}

static EN: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| crate::i18n_en::EN.iter().copied().collect());

/// Parsed locale files, each on first use. Texts are leaked once (a few hundred per language).
static TABLES: LazyLock<Vec<OnceLock<HashMap<u32, &'static str>>>> = LazyLock::new(|| LOCALES.iter().map(|_| OnceLock::new()).collect());

fn table(i: u16) -> Option<&'static HashMap<u32, &'static str>> {
    let cell = TABLES.get(i as usize)?;
    Some(cell.get_or_init(|| {
        let raw: HashMap<String, String> = serde_json::from_str(LOCALES[i as usize].1).unwrap_or_else(|e| {
            log::warn!("i18n: {} unreadable: {e}", LOCALES[i as usize].0);
            HashMap::new()
        });
        raw.into_iter()
            .filter_map(|(k, v)| Some((u32::from_str_radix(&k, 16).ok()?, &*Box::leak(v.into_boxed_str()))))
            .collect()
    }))
}

/// The id of a key: FNV-1a 32 over its UTF-8 bytes (same as the tools in scripts/).
pub fn key_id(key: &str) -> u32 {
    key.bytes().fold(0x811C_9DC5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

/// Looks `key` up in the active non-Turkish language (None when missing).
fn lookup(key: &str) -> Option<&'static str> {
    match active() {
        Lang::Tr | Lang::System => None,
        Lang::En => EN.get(key).copied(),
        Lang::Other(i) => table(i).and_then(|t| t.get(&key_id(key)).copied()).or_else(|| EN.get(key).copied()),
    }
}

/// Translates a Turkish literal. Missing entries fall back to English, then to the Turkish text.
pub fn t(tr: &'static str) -> &'static str {
    if is_tr() {
        return tr;
    }
    match lookup(tr) {
        Some(s) => s,
        None => {
            log::debug!("i18n: missing text for {tr:?}");
            tr
        }
    }
}

/// Inline pair for the rare Turkish word that needs two different English translations
/// (e.g. "Açık" = "Light" theme vs "On" toggle). Other languages key it by the English text.
pub fn tx(tr: &'static str, en: &'static str) -> &'static str {
    match active() {
        Lang::Tr | Lang::System => tr,
        Lang::En => en,
        Lang::Other(i) => table(i).and_then(|t| t.get(&key_id(&format!("en:{en}"))).copied()).unwrap_or(en),
    }
}

/// Translates a dynamic string that may be a known Turkish text (error messages from the
/// lower crates, group labels, built-in tag names...). `"Klasör bulunamadı: C:\x"` is handled by
/// translating the part before the first `": "`. Unknown strings are returned unchanged.
pub fn td(s: &str) -> String {
    if is_tr() {
        return s.to_string();
    }
    if let Some(t) = lookup(s) {
        return t.to_string();
    }
    if let Some((head, rest)) = s.split_once(": ") {
        if let Some(t) = lookup(head) {
            return format!("{t}: {rest}");
        }
    }
    s.to_string()
}

/// A counted / templated text of a non-Turkish, non-English language ("@items_one"...).
fn template(key: &str) -> Option<&'static str> {
    match active() {
        Lang::Other(i) => table(i).and_then(|t| t.get(&key_id(key)).copied()),
        _ => None,
    }
}

/// Percentage in the UI language's order: Turkish "%53", English "53%", French "53 %".
pub fn pct(v: f32) -> String {
    let v = format!("{v:.0}");
    match active() {
        Lang::Tr | Lang::System => format!("%{v}"),
        _ => template("@percent").unwrap_or("{v}%").replace("{v}", &v),
    }
}

fn counted(n: usize, tr: &str, one: &str, other: &str, en_one: &str, en_other: &str) -> String {
    match active() {
        Lang::Tr | Lang::System => tr.replace("{n}", &n.to_string()),
        _ => {
            let fallback = if n == 1 { en_one } else { en_other };
            template(if n == 1 { one } else { other }).unwrap_or(fallback).replace("{n}", &n.to_string())
        }
    }
}

/// "1 öğe" / "5 öğe" (Tr), "1 item" / "5 items" (En).
pub fn items(n: usize) -> String {
    counted(n, "{n} öğe", "@items_one", "@items_other", "{n} item", "{n} items")
}

/// "3 öğe seçildi" / "3 items selected".
pub fn items_selected(n: usize) -> String {
    counted(n, "{n} öğe seçildi", "@selected_one", "@selected_other", "{n} item selected", "{n} items selected")
}

#[macro_export]
macro_rules! t {
    ($s:literal) => {
        $crate::i18n::t($s)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_keys_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for (tr, en) in crate::i18n_en::EN {
            assert!(seen.insert(*tr), "duplicate key {tr:?}");
            assert!(!en.is_empty(), "empty English for {tr:?}");
        }
        assert_eq!(EN.len(), crate::i18n_en::EN.len());
    }

    #[test]
    fn ids_match_the_tools() {
        // python: fnv1a32("Aranacak metin") in scripts → 29b4f21c
        assert_eq!(format!("{:08x}", key_id("Aranacak metin")), "29b4f21c");
    }

    #[test]
    fn codes_round_trip_and_windows_tags_match() {
        assert_eq!(Lang::from_code("tr"), Lang::Tr);
        assert_eq!(Lang::from_code("en"), Lang::En);
        assert_eq!(Lang::from_code("system"), Lang::System);
        assert_eq!(Lang::from_code("xx-YY"), Lang::System);
        assert_eq!(match_tag("tr-TR"), Lang::Tr);
        assert_eq!(match_tag("en-US"), Lang::En);
        for l in Lang::all() {
            assert_eq!(Lang::from_code(l.code()), l);
            assert!(!l.native_name().is_empty());
            assert!(!l.is_rtl());
        }
        assert_eq!(match_tag("ar-SA"), Lang::En);
        if let Some(i) = LOCALES.iter().position(|l| l.0 == "de-DE") {
            assert_eq!(match_tag("de-AT"), Lang::Other(i as u16));
        }
        if let Some(i) = LOCALES.iter().position(|l| l.0 == "zh-Hant") {
            assert_eq!(match_tag("zh-TW"), Lang::Other(i as u16));
        }
        if let Some(i) = LOCALES.iter().position(|l| l.0 == "es-419") {
            assert_eq!(match_tag("es-MX"), Lang::Other(i as u16));
        }
    }

    #[test]
    fn turkish_is_identity_and_english_translates() {
        // One test mutates the global language; keep everything in it.
        set(Lang::Tr);
        for (tr, _) in crate::i18n_en::EN {
            assert_eq!(t(tr), *tr);
        }
        assert_eq!(items(1), "1 öğe");
        assert_eq!(pct(53.), "%53");
        set(Lang::En);
        assert_eq!(t("Yeni klasör"), "New folder");
        assert_eq!(items(1), "1 item");
        assert_eq!(items(3), "3 items");
        assert_eq!(items_selected(2), "2 items selected");
        assert_eq!(pct(53.), "53%");
        assert_eq!(td("Klasör bulunamadı: C:/x"), "Folder not found: C:/x");
        assert_eq!(td("kullanıcı dosyası.txt"), "kullanıcı dosyası.txt");
        assert_eq!(rf_core::format::size(1536), "1.50 KB");
        // every locale file: readable, and a missing text falls back to English
        for i in 0..LOCALES.len() as u16 {
            set(Lang::Other(i));
            assert!(!table(i).unwrap().is_empty(), "{} is empty or unreadable", LOCALES[i as usize].0);
            assert!(!t("Yeni klasör").is_empty());
            assert!(items(5).contains('5'));
        }
        set(Lang::Tr);
        assert_eq!(rf_core::format::size(1536), "1,50 KB");
    }
}
