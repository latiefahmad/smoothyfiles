use std::path::{Path, PathBuf};

/// Where a tab is pointing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Home,
    Path(PathBuf),
    RecycleBin,
    Search { root: PathBuf, query: String },
    Settings,
    /// All files carrying a tag (see `tags::TagStore`).
    Tag(u32),
    /// A Windows library (`.library-ms`), shown as the merged contents of its folders.
    Library { file: PathBuf, name: String },
    /// A folder of the shell namespace without a file-system path (an MTP phone under This PC,
    /// Phone Link's mobile devices, Network…). `parsing` is the desktop-absolute parsing name
    /// the shell takes back; `name` is what it shows (may be empty: then the last part of
    /// `parsing` stands in until the shell has been asked).
    Shell { parsing: String, name: String },
    /// Where the space goes under a folder or drive (the disk usage view).
    DiskUsage(PathBuf),
}

impl Location {
    /// Shell namespaces we show ourselves, as Explorer hands them over (desktop icons, Run box,
    /// links): This PC / Home / Quick access → `Home`, Recycle Bin → `RecycleBin`.
    pub fn from_shell_name(s: &str) -> Option<Location> {
        if let Some(search) = Location::from_search_ms(s) {
            return Some(search);
        }
        let s = s.trim().trim_matches('"').trim_end_matches('\\').to_ascii_lowercase();
        match s.as_str() {
            "::{20d04fe0-3aea-1069-a2d8-08002b30309d}" | "shell:mycomputerfolder" | "::{f874310e-b6b7-47dc-bc84-b9e6b38f5903}"
            | "::{679f85cb-0220-4080-b29b-5540cc05aab6}" | "shell:::{20d04fe0-3aea-1069-a2d8-08002b30309d}" => Some(Location::Home),
            "::{645ff040-5081-101b-9f08-00aa002f954e}" | "shell:recyclebinfolder" | "shell:::{645ff040-5081-101b-9f08-00aa002f954e}" => {
                Some(Location::RecycleBin)
            }
            _ => None,
        }
    }

    /// A Windows search link, as Explorer, the Start menu and other apps hand one to the file
    /// manager: `search-ms:query=rapor&crumb=location:C%3A%5CUsers` (percent-encoded, `crumb`
    /// may repeat; the first `location:` is the folder searched). `None` without a query or a
    /// location.
    pub fn from_search_ms(s: &str) -> Option<Location> {
        let s = s.trim().trim_matches('"');
        let rest = s.get(..10).filter(|p| p.eq_ignore_ascii_case("search-ms:")).map(|_| &s[10..])?;
        let (mut query, mut root) = (None, None);
        for part in rest.split('&') {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let v = percent_decode(v);
            match k.to_ascii_lowercase().as_str() {
                "query" => query = Some(v),
                "crumb" if root.is_none() => {
                    if let Some(loc) = v.get(..9).filter(|p| p.eq_ignore_ascii_case("location:")).map(|_| &v[9..]) {
                        root = Some(PathBuf::from(percent_decode(loc)));
                    }
                }
                _ => {}
            }
        }
        let query = query.filter(|q| !q.trim().is_empty())?;
        Some(Location::Search { root: root?, query: query.trim().to_string() })
    }

    /// The same place with its path tidied (see [`clean_path`]).
    pub fn cleaned(self) -> Location {
        match self {
            Location::Path(p) => Location::Path(clean_path(&p)),
            Location::Search { root, query } => Location::Search { root: clean_path(&root), query },
            Location::DiskUsage(p) => Location::DiskUsage(clean_path(&p)),
            other => other,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Location::Path(p) => Some(p),
            Location::Search { root, .. } => Some(root),
            _ => None,
        }
    }

    /// Parent for the "Up" button. Drive roots go to Home.
    pub fn parent(&self) -> Option<Location> {
        match self {
            // `\\server\share` → `\\server` (lists the server's shares)
            Location::Path(p) if unc_share_root(p).is_some() => unc_share_root(p).map(Location::Path),
            // ftp://host is the top of an FTP site
            Location::Path(p) if is_ftp_root(p) => Some(Location::Home),
            Location::Path(p) => match p.parent() {
                Some(par) if !par.as_os_str().is_empty() => Some(Location::Path(par.to_path_buf())),
                _ => Some(Location::Home),
            },
            Location::Search { root, .. } => Some(Location::Path(root.clone())),
            Location::DiskUsage(p) => Some(Location::Path(p.clone())),
            Location::RecycleBin | Location::Settings | Location::Tag(_) | Location::Library { .. } => {
                Some(Location::Home)
            }
            Location::Shell { parsing, .. } => Some(shell_parent(parsing)),
            Location::Home => None,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Location::Home => "Ana Sayfa".into(),
            Location::RecycleBin => "Geri Dönüşüm Kutusu".into(),
            Location::Settings => "Ayarlar".into(),
            Location::Tag(_) => "Etiket".into(),
            Location::Search { query, .. } => format!("Arama: {query}"),
            Location::Library { name, .. } => name.clone(),
            Location::Shell { parsing, name } if name.is_empty() => shell_last_segment(parsing).to_string(),
            Location::Shell { name, .. } => name.clone(),
            Location::DiskUsage(p) => format!("Disk kullanımı — {}", display_name(p)),
            Location::Path(p) => display_name(p),
        }
    }
}

/// Last part of a shell parsing name (`::{GUID}\Foo` → `Foo`).
pub fn shell_last_segment(parsing: &str) -> &str {
    let t = parsing.trim_end_matches('\\');
    t.rsplit('\\').next().unwrap_or(t)
}

const THIS_PC_PARSING: &str = "::{20d04fe0-3aea-1069-a2d8-08002b30309d}";

/// The folder above a shell parsing name, by its text: `…\Internal storage\DCIM` →
/// `…\Internal storage`. A device's own name starts with `\\?\` (`::{This PC}\\?\usb#…`), so a
/// cut that would land inside that prefix goes to what precedes it. This PC and the roots of
/// the namespace go to Home.
pub fn shell_parent(parsing: &str) -> Location {
    let t = parsing.trim_end_matches('\\');
    let up = match t.rfind('\\') {
        None => "",
        Some(cut) => {
            let head = &t[..cut];
            if head.ends_with(r"\\?") || head.ends_with('\\') {
                head.trim_end_matches(['?', '\\'])
            } else {
                head
            }
        }
    };
    if up.is_empty() || up.eq_ignore_ascii_case(THIS_PC_PARSING) {
        return Location::Home;
    }
    Location::Shell { parsing: up.to_string(), name: String::new() }
}

fn is_ftp_root(p: &Path) -> bool {
    let Some(s) = p.to_str() else { return false };
    let s = s.replace('\\', "/");
    // byte-safe: never slice inside a multi-byte character (C:\Öğrenci)
    s.as_bytes().get(..6).is_some_and(|b| b.eq_ignore_ascii_case(b"ftp://"))
        && s.len() > 6
        && !s[6..].trim_end_matches('/').contains('/')
}

/// For exactly `\\server\share` (optionally with a trailing separator) returns `\\server`.
/// WSL (`\\wsl$`, `\\wsl.localhost`) has no share list: going up from a distro goes Home.
fn unc_share_root(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?.trim_end_matches('\\');
    let rest = s.strip_prefix(r"\\")?;
    if rest.starts_with(['?', '.']) {
        return None;
    }
    let (server, share) = rest.split_once('\\')?;
    if server.eq_ignore_ascii_case("wsl$") || server.eq_ignore_ascii_case("wsl.localhost") {
        return None;
    }
    (!server.is_empty() && !share.is_empty() && !share.contains('\\')).then(|| PathBuf::from(format!(r"\\{server}")))
}

/// "C:\" → "C:", "C:\Users\x" → "x".
pub fn display_name(p: &Path) -> String {
    match p.file_name() {
        Some(n) => redact_credentials(&n.to_string_lossy()),
        None => redact_credentials(p.to_string_lossy().trim_end_matches(['\\', '/'])),
    }
}

/// Drops a password from URL-style text: `ftp://user:pw@host/x` → `ftp://user@host/x`, and a
/// bare `user:pw@host` (a path component) → `user@host`. Anything else is returned unchanged.
pub fn redact_credentials(s: &str) -> String {
    let (scheme, rest) = match s.find("://") {
        Some(i) => (&s[..i + 3], &s[i + 3..]),
        None => ("", s),
    };
    let auth_end = rest.find(['/', '\\']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            match userinfo.find(':') {
                Some(colon) => format!("{scheme}{}{}", &userinfo[..colon], &rest[at..]),
                None => s.to_string(),
            }
        }
        None => s.to_string(),
    }
}

/// Browser-style back/forward history.
#[derive(Debug, Clone)]
pub struct History {
    back: Vec<Location>,
    current: Location,
    forward: Vec<Location>,
}

impl History {
    pub fn new(start: Location) -> Self {
        Self { back: Vec::new(), current: start.cleaned(), forward: Vec::new() }
    }
    pub fn current(&self) -> &Location {
        &self.current
    }
    pub fn navigate(&mut self, to: Location) {
        let to = to.cleaned();
        if to == self.current {
            return;
        }
        let prev = std::mem::replace(&mut self.current, to);
        self.back.push(prev);
        if self.back.len() > 200 {
            self.back.remove(0);
        }
        self.forward.clear();
    }
    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
    pub fn back(&mut self) -> Option<&Location> {
        let prev = self.back.pop()?;
        let cur = std::mem::replace(&mut self.current, prev);
        self.forward.push(cur);
        Some(&self.current)
    }
    /// Back entries, most recent first.
    pub fn back_entries(&self) -> impl Iterator<Item = &Location> {
        self.back.iter().rev()
    }
    /// Forward entries, nearest first.
    pub fn forward_entries(&self) -> impl Iterator<Item = &Location> {
        self.forward.iter().rev()
    }
    /// Goes `n` steps back (1 = like `back`).
    pub fn back_n(&mut self, n: usize) -> bool {
        let mut moved = false;
        for _ in 0..n {
            moved |= self.back().is_some();
        }
        moved
    }
    pub fn forward_n(&mut self, n: usize) -> bool {
        let mut moved = false;
        for _ in 0..n {
            moved |= self.forward().is_some();
        }
        moved
    }
    pub fn forward(&mut self) -> Option<&Location> {
        let next = self.forward.pop()?;
        let cur = std::mem::replace(&mut self.current, next);
        self.back.push(cur);
        Some(&self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_icons_open_here() {
        assert_eq!(Location::from_shell_name("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"), Some(Location::Home));
        assert_eq!(Location::from_shell_name("\"::{645FF040-5081-101B-9F08-00AA002F954E}\""), Some(Location::RecycleBin));
        assert_eq!(Location::from_shell_name("shell:RecycleBinFolder"), Some(Location::RecycleBin));
        assert_eq!(Location::from_shell_name("shell:MyComputerFolder"), Some(Location::Home));
        // Control Panel etc. stay with Explorer
        assert_eq!(Location::from_shell_name("::{26EE0668-A00A-44D7-9371-BEB064C98683}"), None);
        assert_eq!(Location::from_shell_name(r"C:\Windows"), None);
    }

    #[test]
    fn shell_locations_go_up_the_namespace() {
        let sh = |p: &str| Location::Shell { parsing: p.into(), name: String::new() };
        let pc = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
        // a root of the namespace → Home
        assert_eq!(sh("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}").parent(), Some(Location::Home));
        // a device under This PC → Home (our This PC)
        let dev = format!(r"{pc}\\?\usb#vid_04e8&pid_6860#abc#{{6ac27878-a6fa-4155-ba85-f98f491d4f33}}");
        assert_eq!(sh(&dev).parent(), Some(Location::Home));
        // storage and folders on it go up one level at a time
        let storage = format!(r"{dev}\SID-{{10001,,128}}");
        let dcim = format!(r"{storage}\{{0001}}");
        assert_eq!(sh(&dcim).parent(), Some(sh(&storage)));
        assert_eq!(sh(&storage).parent(), Some(sh(&dev)));
        assert_eq!(sh(&format!(r"{storage}\")).parent(), Some(sh(&dev)));
        assert_eq!(sh(r"::{A}\Docs").parent(), Some(sh("::{A}")));
    }

    #[test]
    fn shell_titles() {
        let l = |name: &str| Location::Shell { parsing: r"::{A}\Foo".into(), name: name.into() };
        assert_eq!(l("Internal storage").title(), "Internal storage");
        assert_eq!(l("").title(), "Foo");
        assert_eq!(l("x").path(), None);
        assert_eq!(l("x").cleaned(), l("x"));
    }

    #[test]
    fn non_ascii_paths_do_not_panic() {
        let up = |s: &str| Location::Path(PathBuf::from(s)).parent();
        assert_eq!(up(r"C:\Öğrenci\x"), Some(Location::Path(PathBuf::from(r"C:\Öğrenci"))));
        assert_eq!(up(r"C:\aaç"), Some(Location::Path(PathBuf::from(r"C:\"))));
        assert_eq!(up(r"\\wsl.localhost\Ubuntu"), Some(Location::Home));
        assert_eq!(up("ftp://host"), Some(Location::Home));
    }

    #[test]
    fn redacts() {
        assert_eq!(redact_credentials("ftp://u:pw@h/x"), "ftp://u@h/x");
        assert_eq!(redact_credentials("ftp://u@h/x"), "ftp://u@h/x");
        assert_eq!(redact_credentials("u:pw@h"), "u@h");
        assert_eq!(redact_credentials(r"C:\a@b:c"), r"C:\a@b:c");
    }

    #[test]
    fn unc_up() {
        let up = |s: &str| Location::Path(PathBuf::from(s)).parent();
        assert_eq!(up(r"\\nas\share"), Some(Location::Path(PathBuf::from(r"\\nas"))));
        assert_eq!(up(r"\\nas\share\"), Some(Location::Path(PathBuf::from(r"\\nas"))));
        assert_eq!(up(r"\\nas\share\dir"), Some(Location::Path(PathBuf::from(r"\\nas\share\"))));
        assert_eq!(up(r"C:\x"), Some(Location::Path(PathBuf::from(r"C:\"))));
    }

    #[test]
    fn back_forward() {
        let mut h = History::new(Location::Home);
        h.navigate(Location::Path("C:\\".into()));
        h.navigate(Location::Path("C:\\Users".into()));
        assert_eq!(h.back(), Some(&Location::Path("C:\\".into())));
        assert!(h.can_forward());
        h.navigate(Location::Path("D:\\".into()));
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(&Location::Path("C:\\".into())));
        assert_eq!(h.back(), Some(&Location::Home));
        assert_eq!(h.back(), None);
    }

    #[test]
    fn parent_of_root_is_home() {
        assert_eq!(Location::Path("C:\\".into()).parent(), Some(Location::Home));
        assert_eq!(
            Location::Path("C:\\Users".into()).parent(),
            Some(Location::Path("C:\\".into()))
        );
        assert_eq!(display_name(Path::new("C:\\")), "C:");
    }
}

/// Tidies a path typed, pasted or passed in, the way Explorer reads it: `/` becomes a
/// backslash, doubled separators collapse, `.` and `..` resolve lexically, the drive letter
/// is upper case, and there's no trailing separator except on a root. URLs (`ftp://…`),
/// shell names (`shell:…`, `::{…}`), device / verbatim paths and relative paths are left
/// as they are.
pub fn clean_path(p: &Path) -> PathBuf {
    const B: char = '\\';
    let Some(s) = p.to_str() else { return p.to_path_buf() };
    let lower = s.to_ascii_lowercase();
    let verbatim = s.starts_with("\\\\?\\") || s.starts_with("\\\\.\\");
    if s.contains("://") || lower.starts_with("shell:") || s.starts_with("::") || verbatim {
        return p.to_path_buf();
    }
    let s = s.replace('/', "\\");
    // the prefix: `C:` or a UNC `\\server\share`
    let (prefix, rest) = if let Some(body) = s.strip_prefix("\\\\") {
        let mut it = body.split(B).filter(|x| !x.is_empty());
        match (it.next(), it.next()) {
            (Some(server), Some(share)) => {
                let after_share = body.find(share).map_or(body.len(), |i| i + share.len());
                (format!("{B}{B}{server}{B}{share}"), body[after_share..].to_string())
            }
            (Some(server), None) => return PathBuf::from(format!("{B}{B}{server}")),
            _ => return p.to_path_buf(),
        }
    } else if s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic() {
        (s[..2].to_ascii_uppercase(), s[2..].to_string())
    } else {
        return PathBuf::from(s); // relative: nothing to anchor `..` to
    };
    let mut parts: Vec<&str> = Vec::new();
    for c in rest.split(B) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = prefix;
    let unc = out.starts_with(B);
    if parts.is_empty() && !unc {
        out.push(B);
    }
    for c in parts {
        out.push(B);
        out.push_str(c);
    }
    PathBuf::from(out)
}

#[cfg(test)]
mod clean_tests {
    use super::*;

    #[test]
    fn clean_paths() {
        let c = |s: &str| clean_path(Path::new(s)).to_string_lossy().replace('\\', "|");
        assert_eq!(c("C:/Users/x/./y/../z/"), "C:|Users|x|z");
        assert_eq!(c("c:||a||b".replace('|', "\\").as_str()), "C:|a|b");
        assert_eq!(c("d:"), "D:|");
        assert_eq!(c("D:/"), "D:|");
        assert_eq!(c("C:|..|..".replace('|', "\\").as_str()), "C:|");
        assert_eq!(c("//server/share/a/b/"), "||server|share|a|b");
        assert_eq!(c("||server|share".replace('|', "\\").as_str()), "||server|share");
        assert_eq!(c("||server".replace('|', "\\").as_str()), "||server");
        assert_eq!(c("ftp://host/a/b"), "ftp://host/a/b");
        assert_eq!(c("shell:Downloads"), "shell:Downloads");
        assert_eq!(c("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"), "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}");
        assert_eq!(c("||?|C:|a/b".replace('|', "\\").as_str()), "||?|C:|a/b");
        assert_eq!(c("C:/Fotoğraflar/Tatil"), "C:|Fotoğraflar|Tatil");
        assert_eq!(c("klasör/alt"), "klasör|alt");
    }
}

/// `%XX` escapes (and `+` for a space, as search links write queries) to text; invalid escapes
/// stay as they are.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod search_ms_tests {
    use super::Location;
    use std::path::PathBuf;

    #[test]
    fn search_links_open_a_search() {
        let l = Location::from_search_ms("search-ms:query=rapor&crumb=location:C%3A%5CUsers%5Cozero");
        assert_eq!(l, Some(Location::Search { root: PathBuf::from(r"C:\Users\ozero"), query: "rapor".into() }));
        // plain path, other parameters, a spaced query, the first location wins
        let l = Location::from_search_ms(r#""search-ms:displayname=Ara&crumb=location:D:\İş Klasörü&query=yıl+sonu%20raporu&crumb=location:E:\""#);
        assert_eq!(l, Some(Location::Search { root: PathBuf::from(r"D:\İş Klasörü"), query: "yıl sonu raporu".into() }));
        // through the launch parser too
        assert!(matches!(Location::from_shell_name("SEARCH-MS:query=x&crumb=location:C:%5C"), Some(Location::Search { .. })));
        // no query or no location: not a search we can show
        assert_eq!(Location::from_search_ms("search-ms:crumb=location:C:%5C"), None);
        assert_eq!(Location::from_search_ms("search-ms:query=x"), None);
        assert_eq!(Location::from_search_ms(r"C:\Users"), None);
    }
}
