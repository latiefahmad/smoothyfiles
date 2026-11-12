use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use serde::{Deserialize, Serialize};

use crate::entry::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Created,
    Size,
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

impl SortDir {
    pub fn flip(self) -> Self {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SortSpec {
    pub key: SortKey,
    pub dir: SortDir,
    pub dirs_first: bool,
    /// Files before folders (Files' "sort files first"); both false = mixed.
    pub files_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self { key: SortKey::Name, dir: SortDir::Asc, dirs_first: true, files_first: false }
    }
}

impl SortSpec {
    /// Clicking a column header: same key flips direction, new key starts ascending
    /// (except dates/size which feel more natural newest/biggest first).
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self { dir: self.dir.flip(), ..self }
        } else {
            let dir = match key {
                SortKey::Modified | SortKey::Created | SortKey::Size => SortDir::Desc,
                _ => SortDir::Asc,
            };
            Self { key, dir, ..self }
        }
    }

    pub fn compare(&self, a: &Entry, b: &Entry) -> Ordering {
        if self.dirs_first && a.is_dir() != b.is_dir() {
            return if a.is_dir() { Ordering::Less } else { Ordering::Greater };
        }
        if self.files_first && a.is_dir() != b.is_dir() {
            return if a.is_dir() { Ordering::Greater } else { Ordering::Less };
        }
        let primary = match self.key {
            SortKey::Name => Ordering::Equal,
            SortKey::Modified => a.modified_ms.cmp(&b.modified_ms),
            SortKey::Created => a.created_ms.cmp(&b.created_ms),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Type => a.ext.cmp(&b.ext),
        };
        let ord = primary.then_with(|| natural_cmp(a.shown(), b.shown()));
        match self.dir {
            SortDir::Asc => ord,
            SortDir::Desc => ord.reverse(),
        }
    }

    /// Sorts indices into `entries` (we sort indices, not entries, so re-sorting is cheap).
    pub fn sort_indices(&self, entries: &[Entry], idx: &mut [u32]) {
        idx.sort_unstable_by(|&x, &y| self.compare(&entries[x as usize], &entries[y as usize]));
    }
}

/// Explorer-like "logical" comparison: case-insensitive, digit runs compared numerically
/// ("file2" < "file10"). Allocation-free (it runs ~n·log n times when sorting big folders).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        return natural_ascii(a.as_bytes(), b.as_bytes()).then_with(|| a.cmp(b));
    }
    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b), // stable tiebreak on exact bytes
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((ia, ca)), Some((ib, cb))) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let ea = digit_end(a.as_bytes(), ia);
                    let eb = digit_end(b.as_bytes(), ib);
                    let o = cmp_digits(&a.as_bytes()[ia..ea], &b.as_bytes()[ib..eb]);
                    if o != Ordering::Equal {
                        return o;
                    }
                    while ai.peek().is_some_and(|(i, _)| *i < ea) {
                        ai.next();
                    }
                    while bi.peek().is_some_and(|(i, _)| *i < eb) {
                        bi.next();
                    }
                } else {
                    let (la, lb) = (rank(ca), rank(cb));
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

fn digit_end(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// Numeric comparison of two digit runs of any length (no overflow), then shorter run first.
fn cmp_digits(x: &[u8], y: &[u8]) -> Ordering {
    let tx = &x[x.iter().take_while(|&&c| c == b'0').count()..];
    let ty = &y[y.iter().take_while(|&&c| c == b'0').count()..];
    tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty)).then_with(|| x.len().cmp(&y.len()))
}

fn natural_ascii(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (ca, cb) = (a[i], b[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let (ea, eb) = (digit_end(a, i), digit_end(b, j));
            let o = cmp_digits(&a[i..ea], &b[j..eb]);
            if o != Ordering::Equal {
                return o;
            }
            i = ea;
            j = eb;
        } else {
            let (la, lb) = (rank(ca as char), rank(cb as char));
            if la != lb {
                return la.cmp(&lb);
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

static TURKISH: AtomicBool = AtomicBool::new(false);

/// Turkish collation: capital `I` is the capital of dotless `ı` (as Explorer sorts on Turkish
/// Windows). Set once at startup from the user locale.
pub fn set_turkish_collation(on: bool) {
    TURKISH.store(on, AtomicOrdering::Relaxed);
}

pub(crate) fn turkish() -> bool {
    TURKISH.load(AtomicOrdering::Relaxed)
}

/// Uppercase. On a Turkish locale, words that are visibly Turkish (contain ç ğ ı ö ş ü or
/// their capitals, or İ) use Turkish rules (i → İ); other words — English file names such as
/// "windows", "readme" — use standard rules, so they don't turn into "WİNDOWS".
pub fn upper(s: &str) -> String {
    upper_with(s, turkish())
}

/// Lowercase; see `upper` for when Turkish rules (I → ı) apply. İ always becomes plain i.
pub fn lower(s: &str) -> String {
    lower_with(s, turkish())
}

fn is_turkish_word(w: &str) -> bool {
    w.chars().any(|c| matches!(c, 'ç' | 'ğ' | 'ı' | 'ö' | 'ş' | 'ü' | 'Ç' | 'Ğ' | 'İ' | 'Ö' | 'Ş' | 'Ü'))
}

/// Applies `f(word, turkish_rules)` to each alphanumeric run, keeping separators as they are.
fn map_words(s: &str, turkish: bool, f: impl Fn(&str, bool) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&f(word, turkish && is_turkish_word(word)));
            word.clear();
        }
    };
    for c in s.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn upper_char(c: char, tr: bool) -> String {
    match c {
        'i' if tr => "İ".into(),
        'ı' => "I".into(),
        _ => c.to_uppercase().collect(),
    }
}

fn lower_char(c: char, tr: bool) -> String {
    match c {
        'I' if tr => "ı".into(),
        'İ' => "i".into(),
        _ => c.to_lowercase().collect(),
    }
}

fn upper_with(s: &str, turkish: bool) -> String {
    map_words(s, turkish, |w, tr| w.chars().map(|c| upper_char(c, tr)).collect())
}

fn lower_with(s: &str, turkish: bool) -> String {
    map_words(s, turkish, |w, tr| w.chars().map(|c| lower_char(c, tr)).collect())
}

/// "hELLO wORLD-foo" → "Hello World-Foo", with the same per-word Turkish rule.
pub fn title(s: &str) -> String {
    title_with(s, turkish())
}

fn title_with(s: &str, turkish: bool) -> String {
    map_words(s, turkish, |w, tr| {
        let mut cs = w.chars();
        match cs.next() {
            Some(first) => upper_char(first, tr) + &cs.map(|c| lower_char(c, tr)).collect::<String>(),
            None => String::new(),
        }
    })
}

/// Case-insensitive sort weight of a (non-digit) character.
/// - Punctuation and symbols (ASCII) before letters, as in Explorer.
/// - Latin letters in alphabet order, with the Turkish letters right after their base letter
///   (c ç d … g ğ h ı i … o ö p … s ş t u ü v) instead of after `z` by code point.
/// - Everything else after, by code point.
#[inline]
fn rank(c: char) -> u32 {
    const LETTERS: u32 = 0x1000;
    let r = |b: char| LETTERS + (b as u32 - 'a' as u32) * 4;
    let c = match c {
        'I' if TURKISH.load(AtomicOrdering::Relaxed) => 'ı',
        'İ' => 'i',
        _ => c,
    };
    let l = if c.is_ascii() { c.to_ascii_lowercase() } else { c.to_lowercase().next().unwrap_or(c) };
    match l {
        'a'..='z' => r(l),
        'ç' => r('c') + 2,
        'ğ' => r('g') + 2,
        'ı' => r('i') - 2,
        'ö' => r('o') + 2,
        'ş' => r('s') + 2,
        'ü' => r('u') + 2,
        _ if l.is_ascii() => l as u32,
        _ => 0x10000 + l as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "a", "B", "file02"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a", "B", "file1", "File2", "file02", "file10"]);
        // non-ASCII path agrees with the ASCII fast path
        let mut t = vec!["dosya10", "Çalışma2", "dosya2", "çalışma10", "Dosya1"];
        t.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(t, vec!["Çalışma2", "çalışma10", "Dosya1", "dosya2", "dosya10"]);
        assert_eq!(natural_cmp("x99999999999999999999999", "x100000000000000000000000"), Ordering::Less);
        // Turkish letters next to their base letter, not after z
        let mut tr = vec!["Zebra", "Çanta", "Dosya", "Ceket", "Şeker", "Su", "Tuz", "Ödev", "Ov", "Üzüm", "Uçak", "Vazo", "ılık", "iki", "hava", "ğ", "h"];
        tr.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(tr, vec!["Ceket", "Çanta", "Dosya", "ğ", "h", "hava", "ılık", "iki", "Ov", "Ödev", "Su", "Şeker", "Tuz", "Uçak", "Üzüm", "Vazo", "Zebra"]);
        // Turkish case rules only when enabled
        // (explicit flag: tests must not flip the global while others run in parallel)
        // Turkish words get Turkish rules, English-looking words standard ones
        assert_eq!(upper_with("işlem ılık windows", true), "İŞLEM ILIK WINDOWS");
        assert_eq!(lower_with("İZMİR WINDOWS README", true), "izmir windows readme");
        assert_eq!(upper_with("istanbul", false), "ISTANBUL");
        assert_eq!(title_with("şİMDİ readME", true), "Şimdi Readme");
        // consistent total order between the ASCII fast path and the general path
        let words = ["a", "B", "_x", "ç", "c", "I", "ı", "i", "İ", "z", "10", "9", "~", "é", "e"];
        for x in words {
            for y in words {
                assert_eq!(natural_cmp(x, y), natural_cmp(y, x).reverse(), "{x} {y}");
            }
        }
    }

    #[test]
    fn dirs_first_and_toggle() {
        let es = vec![
            Entry::new("z.txt", EntryKind::File, 10, 5, 0, 0),
            Entry::new("adir", EntryKind::Dir, 0, 1, 0, 0),
            Entry::new("b.txt", EntryKind::File, 99, 9, 0, 0),
        ];
        let mut idx: Vec<u32> = (0..3).collect();
        SortSpec::default().sort_indices(&es, &mut idx);
        assert_eq!(idx, vec![1, 2, 0]);
        let s = SortSpec::default().toggled(SortKey::Size);
        assert_eq!(s.dir, SortDir::Desc);
        s.sort_indices(&es, &mut idx);
        assert_eq!(idx, vec![1, 2, 0]);
        let s = s.toggled(SortKey::Size);
        assert_eq!(s.dir, SortDir::Asc);
    }
}
