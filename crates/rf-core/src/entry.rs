use std::sync::Arc;

/// Windows file attribute bits we care about (same values as FILE_ATTRIBUTE_*).
pub mod attrs {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const COMPRESSED: u32 = 0x800;
    pub const OFFLINE: u32 = 0x1000;
    pub const ENCRYPTED: u32 = 0x4000;
    pub const RECALL_ON_OPEN: u32 = 0x40000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Dir,
    File,
}

/// One item inside a directory listing. The parent path is stored once on the listing,
/// so an entry only carries its own name (keeps 100k-item folders compact).
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Arc<str>,
    /// Lower-case extension without the dot ("" for none / directories).
    pub ext: Arc<str>,
    pub kind: EntryKind,
    pub size: u64,
    /// Unix time in milliseconds (0 = unknown).
    pub modified_ms: i64,
    pub created_ms: i64,
    pub attrs: u32,
    /// The name Explorer shows for a folder with a localized name (desktop.ini
    /// `LocalizedResourceName`: Downloads → "İndirilenler"). The real name stays in `name`.
    pub label: Option<Arc<str>>,
}

impl Entry {
    pub fn new(name: &str, kind: EntryKind, size: u64, modified_ms: i64, created_ms: i64, attrs: u32) -> Self {
        let ext = match name.rfind('.') {
            Some(i) if kind == EntryKind::File && i > 0 && i + 1 < name.len() => shared_ext(&name[i + 1..]),
            _ => shared_ext(""),
        };
        Self { name: name.into(), ext, kind, size, modified_ms, created_ms, attrs, label: None }
    }

    /// The name to show (and sort and type-select by): the localized one when there is one.
    pub fn shown(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn is_hidden(&self) -> bool {
        self.attrs & attrs::HIDDEN != 0
    }
    pub fn is_system(&self) -> bool {
        self.attrs & attrs::SYSTEM != 0
    }
    pub fn is_symlink(&self) -> bool {
        self.attrs & attrs::REPARSE_POINT != 0
    }
    /// Cloud placeholder not present locally (OneDrive "online-only").
    pub fn is_cloud_placeholder(&self) -> bool {
        self.attrs & (attrs::OFFLINE | attrs::RECALL_ON_OPEN | attrs::RECALL_ON_DATA_ACCESS) != 0
    }
    /// Name without the extension (for rename selection).
    pub fn stem_len(&self) -> usize {
        // From the original name: `ext` is lower-cased and may differ in byte length
        // (e.g. "İ" → "i̇"), so never derive offsets from it.
        if self.ext.is_empty() {
            return self.name.len();
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 => i,
            _ => self.name.len(),
        }
    }
}

/// Visibility filter applied on top of a listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub show_hidden: bool,
    pub show_system: bool,
    /// Names starting with "." count as hidden (Settings: "show dot files" off).
    pub dot_hidden: bool,
    /// Case-insensitive substring on the name; empty = no filter.
    pub query: String,
    /// Open/Save dialog: files are shown only when they match these patterns ("*.txt;*.md");
    /// empty = all. Folders always show.
    pub patterns: String,
    /// Open/Save dialog picking a folder: files are hidden.
    pub folders_only: bool,
}

impl Filter {
    pub fn accepts(&self, e: &Entry) -> bool {
        if !self.show_hidden && (e.is_hidden() || (self.dot_hidden && e.name.starts_with('.'))) {
            return false;
        }
        if !self.show_system && e.is_system() && e.is_hidden() {
            return false;
        }
        if !e.is_dir() && (self.folders_only || (!self.patterns.is_empty() && !matches_patterns(&self.patterns, &e.name))) {
            return false;
        }
        // the shown (localized) name or the real one: "İndir" and "Down" both find Downloads
        if !self.query.is_empty() && !contains_ignore_case(&e.name, &self.query) && !e.label.as_deref().is_some_and(|l| contains_ignore_case(l, &self.query)) {
            return false;
        }
        true
    }
}

/// Does `name` match one of the patterns of `spec` ("*.txt;*.md", "*.*", "rapor-??.pdf")?
/// Case-insensitive, like Windows.
pub fn matches_patterns(spec: &str, name: &str) -> bool {
    let name: Vec<char> = fold_case(name).chars().collect();
    spec.split(';').map(|p| fold_case(p.trim())).filter(|p| !p.is_empty()).any(|p| p == "*" || p == "*.*" || glob(&p.chars().collect::<Vec<_>>(), &name))
}

/// '*' any run, '?' one character.
fn glob(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_ascii() && haystack.is_ascii() {
        let n = needle.as_bytes();
        haystack.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        fold_case(haystack).contains(&fold_case(needle))
    }
}

/// Lenient case folding for matching typed text against names: lowercase, drop the combining
/// dot `"İ".to_lowercase()` produces (so "işlem" finds "İşlem"), and treat dotless `ı` as `i`
/// (so "ilik" finds "ılık" and "ımg" finds "IMG", whichever key the user's layout has).
pub fn fold_case(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .filter(|&c| c != '\u{307}')
        .map(|c| if c == 'ı' { 'i' } else { c })
        .collect()
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    #[test]
    fn turkish_matching() {
        assert!(contains_ignore_case("İşlem raporu.txt", "işlem"));
        assert!(contains_ignore_case("ılık.txt", "ilik"));
        assert!(contains_ignore_case("IMG_001.jpg", "ımg"));
        assert!(contains_ignore_case("Şeker.txt", "şek"));
        assert!(!contains_ignore_case("Şeker.txt", "sek"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_parsing() {
        assert_eq!(&*Entry::new("a.TXT", EntryKind::File, 0, 0, 0, 0).ext, "txt");
        assert_eq!(&*Entry::new(".gitignore", EntryKind::File, 0, 0, 0, 0).ext, "");
        assert_eq!(&*Entry::new("noext", EntryKind::File, 0, 0, 0, 0).ext, "");
        assert_eq!(&*Entry::new("trailing.", EntryKind::File, 0, 0, 0, 0).ext, "");
        assert_eq!(&*Entry::new("dir.d", EntryKind::Dir, 0, 0, 0, 0).ext, "");
        assert_eq!(Entry::new("photo.jpeg", EntryKind::File, 0, 0, 0, 0).stem_len(), 5);
        // lower-casing changes byte length: must not underflow or split a char
        let e = Entry::new("x.İİİ", EntryKind::File, 0, 0, 0, 0);
        assert_eq!(&e.name[..e.stem_len()], "x");
        let k = Entry::new("a.\u{212A}", EntryKind::File, 0, 0, 0, 0);
        assert_eq!(&k.name[..k.stem_len()], "a");
    }

    #[test]
    fn filter_hidden_and_query() {
        let hidden = Entry::new("h", EntryKind::File, 0, 0, 0, attrs::HIDDEN);
        let normal = Entry::new("Rapor.pdf", EntryKind::File, 0, 0, 0, 0);
        let f = Filter::default();
        assert!(!f.accepts(&hidden));
        assert!(f.accepts(&normal));
        assert!(matches_patterns("*.txt;*.MD", "Notlar.md") && !matches_patterns("*.txt", "a.rs") && matches_patterns("*.*", "README"));
        assert!(matches_patterns("rapor-??.pdf", "RAPOR-01.pdf") && !matches_patterns("rapor-??.pdf", "rapor-1.pdf"));
        let f = Filter { query: "rap".into(), show_hidden: true, ..Default::default() };
        assert!(f.accepts(&normal));
        assert!(!f.accepts(&hidden));
        assert!(contains_ignore_case("Çalışma", "çalış"));
    }
}

/// One `Arc` per extension and listing thread instead of one per entry: a folder of 100,000
/// photos holds a handful of different extensions, each then counted, not copied.
fn shared_ext(raw: &str) -> Arc<str> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static EXTS: RefCell<HashMap<Box<str>, Arc<str>>> = RefCell::new(HashMap::new());
    }
    // most extensions are lower-case already: no allocation to look them up
    let lower;
    let key: &str = if raw.bytes().any(|b| b.is_ascii_uppercase()) || !raw.is_ascii() {
        lower = raw.to_lowercase();
        &lower
    } else {
        raw
    };
    EXTS.with(|m| {
        let mut m = m.borrow_mut();
        if let Some(a) = m.get(key) {
            return a.clone();
        }
        let a: Arc<str> = key.into();
        // long-tail extensions (hashes, numbered parts) must not grow it without bound
        if m.len() < 4096 {
            m.insert(key.into(), a.clone());
        }
        a
    })
}
