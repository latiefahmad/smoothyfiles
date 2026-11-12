//! Bulk rename: pure name computation + validation (no I/O).
//!
//! [`compute`] maps every input name to its new name (or a [`RenameError`]); duplicates between
//! the new names, collisions with sibling files that are not part of the batch, Windows-invalid
//! characters, reserved device names and empty names are all reported per item.
//! [`plan`] turns the result into a swap-safe sequence of renames.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use regex::RegexBuilder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameItem {
    pub name: String,
    pub is_dir: bool,
    /// Modified time, ms since the Unix epoch (used by `{date}`).
    pub modified_ms: i64,
}

impl RenameItem {
    pub fn new(name: impl Into<String>, is_dir: bool, modified_ms: i64) -> Self {
        Self { name: name.into(), is_dir, modified_ms }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseKind {
    Lower,
    Upper,
    Title,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    FindReplace { find: String, replace: String, case_insensitive: bool, regex: bool },
    /// `{n}` / `{n:03}` counter, `{name}` stem, `{ext}` (no dot), `{date}` (YYYY-MM-DD).
    Pattern { pattern: String, start: i64, step: i64 },
    Case(CaseKind),
    PrefixSuffix { prefix: String, suffix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameSpec {
    pub mode: Mode,
    /// Apply the mode to the whole name (including the extension) instead of the stem only.
    pub include_ext: bool,
    /// Names of sibling entries that are NOT part of the batch (collision check, case-insensitive).
    pub existing: Vec<String>,
}

impl RenameSpec {
    pub fn new(mode: Mode) -> Self {
        Self { mode, include_ext: false, existing: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenameError {
    #[error("Ad boş olamaz")]
    Empty,
    #[error("Geçersiz karakter: < > : \" / \\ | ? * veya denetim karakteri")]
    InvalidChars,
    #[error("Ad nokta veya boşlukla bitemez")]
    TrailingDotSpace,
    #[error("Ayrılmış Windows adı")]
    Reserved,
    #[error("Ad çok uzun")]
    TooLong,
    #[error("Aynı ada çıkan başka bir öğe var")]
    Duplicate,
    #[error("Klasörde bu ada sahip başka bir öğe var")]
    Exists,
    #[error("Geçersiz düzenli ifade: {0}")]
    BadRegex(String),
}

/// (stem, ext-with-dot). A leading dot alone (".gitignore") is not an extension.
pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn title_case(s: &str) -> String {
    crate::sort::title(s)
}

fn format_date(ms: i64) -> String {
    use chrono::{DateTime, Local};
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Expands `{n}`, `{n:03}`, `{name}`, `{ext}`, `{date}`; unknown braces are kept literally.
fn expand_pattern(pattern: &str, n: i64, stem: &str, ext: &str, date: &str) -> String {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let tok = &after[..close];
        let mut handled = true;
        match tok {
            "name" => out.push_str(stem),
            "ext" => out.push_str(ext.trim_start_matches('.')),
            "date" => out.push_str(date),
            "n" => {
                let _ = write!(out, "{n}");
            }
            t if t.starts_with("n:") => match t[2..].parse::<usize>() {
                Ok(w) => {
                    let _ = write!(out, "{n:0w$}", w = w.min(32));
                }
                Err(_) => handled = false,
            },
            _ => handled = false,
        }
        if !handled {
            out.push('{');
            out.push_str(tok);
            out.push('}');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validates a single file name against Windows rules.
pub fn validate(name: &str) -> Result<(), RenameError> {
    if name.trim().is_empty() {
        return Err(RenameError::Empty);
    }
    if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32) {
        return Err(RenameError::InvalidChars);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(RenameError::TrailingDotSpace);
    }
    let base = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
    if RESERVED.contains(&base.as_str()) {
        return Err(RenameError::Reserved);
    }
    if name.chars().count() > 255 {
        return Err(RenameError::TooLong);
    }
    Ok(())
}

/// The Find/Replace pattern, built once for all the items (not per item: with thousands of
/// files selected that was a regex build per file per keystroke).
fn find_regex(spec: &RenameSpec) -> Option<Result<regex::Regex, RenameError>> {
    let Mode::FindReplace { find, case_insensitive, regex, .. } = &spec.mode else { return None };
    if find.is_empty() {
        return None;
    }
    let pat = if *regex { find.clone() } else { regex::escape(find) };
    Some(RegexBuilder::new(&pat).case_insensitive(*case_insensitive).build().map_err(|e| RenameError::BadRegex(e.to_string().lines().last().unwrap_or("").trim().to_string())))
}

fn transform(item: &RenameItem, idx: usize, spec: &RenameSpec, re: Option<&Result<regex::Regex, RenameError>>) -> Result<String, RenameError> {
    let (stem, ext) = split_ext(&item.name, item.is_dir);
    let (target, tail): (&str, &str) = if spec.include_ext { (&item.name, "") } else { (stem, ext) };
    let body = match &spec.mode {
        Mode::FindReplace { replace, regex, .. } => {
            if let Some(re) = re {
                let re = re.as_ref().map_err(Clone::clone)?;
                if *regex {
                    re.replace_all(target, replace.as_str()).into_owned()
                } else {
                    re.replace_all(target, regex::NoExpand(replace.as_str())).into_owned()
                }
            } else {
                target.to_string()
            }
        }
        Mode::Pattern { pattern, start, step } => {
            let n = start.saturating_add(step.saturating_mul(idx as i64));
            let name_tok = if spec.include_ext { item.name.as_str() } else { stem };
            // (the date is formatted only when the pattern uses it)
            let date = if pattern.contains("{date") { format_date(item.modified_ms) } else { String::new() };
            expand_pattern(pattern, n, name_tok, ext, &date)
        }
        Mode::Case(k) => {
            // The extension (when included) is never cased with locale rules: ".AVI" must
            // become ".avi", not ".avı".
            let (words, ext_part) = if spec.include_ext { split_ext(target, item.is_dir) } else { (target, "") };
            let words = match k {
                CaseKind::Lower => crate::sort::lower(words),
                CaseKind::Upper => crate::sort::upper(words),
                CaseKind::Title => title_case(words),
            };
            let ext_part = match k {
                CaseKind::Upper => ext_part.to_ascii_uppercase(),
                _ => ext_part.to_ascii_lowercase(),
            };
            format!("{words}{ext_part}")
        }
        Mode::PrefixSuffix { prefix, suffix } => format!("{prefix}{target}{suffix}"),
    };
    Ok(format!("{body}{tail}"))
}

/// Computes new names. The result has the same length/order as `items`; an unchanged name is `Ok(old)`.
pub fn compute(items: &[RenameItem], spec: &RenameSpec) -> Vec<Result<String, RenameError>> {
    let re = find_regex(spec);
    let mut out: Vec<Result<String, RenameError>> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let n = transform(it, i, spec, re.as_ref())?;
            validate(&n)?;
            Ok(n)
        })
        .collect();

    let mut count: HashMap<String, usize> = HashMap::new();
    for n in out.iter().flatten() {
        *count.entry(n.to_lowercase()).or_default() += 1;
    }
    let existing: HashSet<String> = spec.existing.iter().map(|s| s.to_lowercase()).collect();
    for r in out.iter_mut() {
        if let Ok(n) = r {
            let k = n.to_lowercase();
            if count.get(&k).copied().unwrap_or(0) > 1 {
                *r = Err(RenameError::Duplicate);
            } else if existing.contains(&k) {
                *r = Err(RenameError::Exists);
            }
        }
    }
    out
}

/// One rename inside a single folder (names only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub from: String,
    pub to: String,
}

/// Turns `(old, new)` pairs into an ordered, swap-safe list of renames. Every pair whose target
/// equals another batch member's current name goes through a temporary name (`tmp(i)`, which must
/// not exist in the folder): first all moves-out, then the plain moves, then all moves-in.
/// Unchanged pairs are skipped; case-only renames go direct.
pub fn plan(pairs: &[(String, String)], tmp: impl Fn(usize) -> String) -> Vec<Step> {
    let changed: Vec<(usize, &(String, String))> = pairs.iter().enumerate().filter(|(_, (a, b))| a != b).collect();
    let olds: HashSet<String> = changed.iter().map(|(_, (a, _))| a.to_lowercase()).collect();
    let (mut out, mut direct, mut back) = (Vec::new(), Vec::new(), Vec::new());
    for (i, (old, new)) in changed {
        let case_only = old.to_lowercase() == new.to_lowercase();
        if !case_only && olds.contains(&new.to_lowercase()) {
            let t = tmp(i);
            out.push(Step { from: old.clone(), to: t.clone() });
            back.push(Step { from: t, to: new.clone() });
        } else {
            direct.push(Step { from: old.clone(), to: new.clone() });
        }
    }
    out.extend(direct);
    out.extend(back);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: &[&str]) -> Vec<RenameItem> {
        n.iter().map(|s| RenameItem::new(*s, false, 0)).collect()
    }
    fn ok(v: Vec<Result<String, RenameError>>) -> Vec<String> {
        v.into_iter().map(|r| r.unwrap()).collect()
    }
    fn pat(p: &str, start: i64, step: i64) -> RenameSpec {
        RenameSpec::new(Mode::Pattern { pattern: p.into(), start, step })
    }
    fn fr(f: &str, r: &str, ci: bool, regex: bool) -> RenameSpec {
        RenameSpec::new(Mode::FindReplace { find: f.into(), replace: r.into(), case_insensitive: ci, regex })
    }
    fn st(a: &str, b: &str) -> Step {
        Step { from: a.into(), to: b.into() }
    }

    #[test]
    fn counter_padding_and_step() {
        let r = ok(compute(&items(&["a.jpg", "b.jpg", "c.png"]), &pat("Tatil_{n:03}", 1, 1)));
        assert_eq!(r, ["Tatil_001.jpg", "Tatil_002.jpg", "Tatil_003.png"]);
        let r = ok(compute(&items(&["a.txt", "b.txt"]), &pat("{n:02}-{name}", 10, 5)));
        assert_eq!(r, ["10-a.txt", "15-b.txt"]);
        let r = ok(compute(&items(&["x.txt", "y.txt"]), &pat("f{n}", 99, 1)));
        assert_eq!(r, ["f99.txt", "f100.txt"]);
    }

    #[test]
    fn pattern_ext_and_unknown_tokens() {
        assert_eq!(ok(compute(&items(&["a.b"]), &pat("{ext}{foo}", 1, 1))), ["b{foo}.b"]);
        let mut s = pat("{name}.{ext}", 1, 1);
        s.include_ext = true;
        assert_eq!(ok(compute(&items(&["a.b"]), &s)), ["a.b.b"]);
    }

    #[test]
    fn date_token() {
        let it = vec![RenameItem::new("a.txt", false, 1_700_000_000_000)];
        let r = ok(compute(&it, &pat("{date}", 1, 1)));
        assert!(r[0].ends_with(".txt") && r[0].len() == "2023-11-14.txt".len(), "{r:?}");
    }

    #[test]
    fn find_replace_plain_and_case() {
        assert_eq!(ok(compute(&items(&["IMG_1.jpg", "img_2.jpg"]), &fr("img", "Foto", true, false))), ["Foto_1.jpg", "Foto_2.jpg"]);
        assert_eq!(ok(compute(&items(&["IMG_1.jpg", "img_2.jpg"]), &fr("img", "Foto", false, false))), ["IMG_1.jpg", "Foto_2.jpg"]);
        // '$' is literal in plain mode
        assert_eq!(ok(compute(&items(&["a.txt"]), &fr("a", "$1", false, false))), ["$1.txt"]);
        // extension untouched by default
        assert_eq!(ok(compute(&items(&["txt.txt"]), &fr("txt", "doc", false, false))), ["doc.txt"]);
        let mut s = fr("txt", "md", false, false);
        s.include_ext = true;
        assert_eq!(ok(compute(&items(&["txt.txt"]), &s)), ["md.md"]);
    }

    #[test]
    fn regex_groups() {
        let s = fr(r"(\d{4})-(\d{2})-(\d{2})", "$3.$2.$1", false, true);
        assert_eq!(ok(compute(&items(&["rapor 2024-03-15.pdf"]), &s)), ["rapor 15.03.2024.pdf"]);
        assert!(matches!(compute(&items(&["a"]), &fr("(", "", false, true))[0], Err(RenameError::BadRegex(_))));
    }

    #[test]
    fn case_modes() {
        let c = |k| RenameSpec::new(Mode::Case(k));
        assert_eq!(ok(compute(&items(&["Hello World.TXT"]), &c(CaseKind::Upper))), ["HELLO WORLD.TXT"]);
        assert_eq!(ok(compute(&items(&["Hello World.TXT"]), &c(CaseKind::Lower))), ["hello world.TXT"]);
        assert_eq!(ok(compute(&items(&["hELLO wORLD-foo_bar.txt"]), &c(CaseKind::Title))), ["Hello World-Foo_Bar.txt"]);
        let mut s = c(CaseKind::Lower);
        s.include_ext = true;
        assert_eq!(ok(compute(&items(&["A.TXT"]), &s)), ["a.txt"]);
    }

    #[test]
    fn prefix_suffix() {
        let s = RenameSpec::new(Mode::PrefixSuffix { prefix: "yeni_".into(), suffix: "_v2".into() });
        assert_eq!(ok(compute(&items(&["a.txt", ".env"]), &s)), ["yeni_a_v2.txt", "yeni_.env_v2"]);
    }

    #[test]
    fn conflicts() {
        let r = compute(&items(&["a.txt", "b.txt"]), &pat("same", 1, 1));
        assert_eq!(r, vec![Err(RenameError::Duplicate), Err(RenameError::Duplicate)]);
        // case-insensitive duplicate: "a" -> "B" collides with existing batch target "b"
        let r = compute(&items(&["a", "b"]), &fr("a", "B", false, false));
        assert_eq!(r[0], Err(RenameError::Duplicate));
        assert_eq!(compute(&items(&["a.txt"]), &pat("x:y", 1, 1))[0], Err(RenameError::InvalidChars));
        assert_eq!(compute(&items(&["a"]), &pat("con", 1, 1))[0], Err(RenameError::Reserved));
        assert_eq!(validate("NUL.txt"), Err(RenameError::Reserved));
        assert_eq!(validate("console"), Ok(()));
        assert_eq!(compute(&items(&["a"]), &pat("", 1, 1))[0], Err(RenameError::Empty));
        assert_eq!(validate("a."), Err(RenameError::TrailingDotSpace));
        let mut s = pat("keep", 1, 1);
        s.existing = vec!["KEEP".into()];
        assert_eq!(compute(&items(&["a"]), &s)[0], Err(RenameError::Exists));
    }

    #[test]
    fn swap_names_are_valid() {
        // Reversing a<->b: targets equal each other's old names but are distinct, so no conflict.
        let r = compute(&items(&["a", "b"]), &pat("{n}", 2, -1));
        assert_eq!(ok(r), ["2", "1"]);
        let pairs = vec![("a".to_string(), "b".to_string()), ("b".to_string(), "a".to_string())];
        let steps = plan(&pairs, |i| format!("~tmp{i}"));
        assert_eq!(steps, vec![st("a", "~tmp0"), st("b", "~tmp1"), st("~tmp0", "b"), st("~tmp1", "a")]);
    }

    #[test]
    fn plan_chain_and_simple() {
        let pairs = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("x".into(), "y".into()),
            ("same".into(), "same".into()),
        ];
        let steps = plan(&pairs, |i| format!("~t{i}"));
        let mut fs: HashSet<String> = ["a", "b", "x", "same"].iter().map(|s| s.to_string()).collect();
        for Step { from, to } in steps {
            assert!(fs.remove(&from), "missing {from}");
            assert!(fs.insert(to.clone()), "overwrite {to}");
        }
        let want: HashSet<String> = ["b", "c", "y", "same"].iter().map(|s| s.to_string()).collect();
        assert_eq!(fs, want);
        assert_eq!(plan(&[("a.txt".into(), "A.txt".into())], |i| format!("~t{i}")), vec![st("a.txt", "A.txt")]);
    }
}
