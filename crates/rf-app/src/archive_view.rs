//! Browsing inside archives like folders (Files' "browsable archives"): a path such as
//! `C:\x\photos.zip\2024\summer` lists the archive's `2024/summer` entries. Listings are cached
//! per archive version; opening or copying an item extracts just that item into a new temp
//! folder and uses the real file from there.
//!
//! Every function here touches the disk: call them off the UI thread.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rf_core::{Entry, EntryKind};

use crate::t;

/// Archive types that open like folders (single-stream .gz/.xz/.zst files open normally).
pub fn is_browsable(p: &Path) -> bool {
    let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    [".zip", ".7z", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.bz2", ".tbz2", ".tbz", ".jar", ".apk", ".nupkg", ".vsix", ".rar", ".iso"]
        .iter()
        .any(|s| n.ends_with(s))
        // first part of a split archive (name.7z.001, name.zip.001…)
        || (n.ends_with(".001") && rf_archive::is_supported(p))
}

/// WinRAR's Rar.exe (looked up once; RAR creation needs it).
pub fn winrar() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(rf_archive::rar_exe).clone()
}

// ---------- passwords (remembered for this session only, never written to disk) ----------

static PASSWORDS: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);

pub fn set_password(archive: &Path, pw: Option<String>) {
    {
        let mut g = PASSWORDS.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        match pw {
            Some(p) => {
                m.insert(archive.to_path_buf(), p);
            }
            None => {
                m.remove(archive);
            }
        }
    }
    // a listing made without the password may be incomplete (7z hides even names)
    if let Some(c) = LISTINGS.lock().unwrap().as_mut() {
        c.remove(archive);
    }
}

pub fn options(archive: &Path) -> rf_archive::Options {
    rf_archive::Options { password: PASSWORDS.lock().unwrap().as_ref().and_then(|m| m.get(archive).cloned()), ..Default::default() }
}

/// Prefix of a listing error that asks for the archive's password.
pub const PASSWORD_MARK: &str = "\u{1}pw:";

/// `Some(true)` = a password was given but is wrong, `Some(false)` = one is needed.
pub fn password_problem(e: &anyhow::Error) -> Option<bool> {
    e.chain().find_map(|c| c.downcast_ref::<rf_archive::PasswordError>()).map(|p| matches!(p, rf_archive::PasswordError::Wrong))
}

/// Before browsing a protected archive: `Some(false)` = a password is needed, `Some(true)` =
/// the remembered one is wrong (and is forgotten), `None` = go ahead. Disk access.
pub fn password_gate(archive: &Path) -> Option<bool> {
    if !rf_archive::needs_password(archive).unwrap_or(false) {
        return None;
    }
    let opts = options(archive);
    if opts.password.is_none() {
        return Some(false);
    }
    match rf_archive::check_password(archive, &opts) {
        Err(e) if password_problem(&e).is_some() => {
            set_password(archive, None);
            Some(true)
        }
        _ => None,
    }
}

/// Needs a password that we don't have yet? (cheap probe; disk access)
pub fn needs_password_now(archive: &Path) -> bool {
    options(archive).password.is_none() && rf_archive::needs_password(archive).unwrap_or(false)
}

/// Paste / drop into an archive's folder view: adds `sources` under `inner`.
pub fn add_into(cx: &mut gpui_kit::App, archive: PathBuf, inner: PathBuf, sources: Vec<PathBuf>) {
    let title = format!("{} {} → {}", t!("Arşive ekleniyor:"), crate::i18n::items(sources.len()), rf_core::history::display_name(&archive));
    let inner_dir = inner.to_string_lossy().replace('\\', "/").trim_matches('/').to_string();
    crate::status_center::start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let opts = options(&archive);
        let mut prog = crate::status_center::archive_progress_pub(&t);
        match rf_archive::add(&archive, &sources, &inner_dir, &opts, &mut prog, &|| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

/// F2 inside an archive: renames the entry (a folder renames everything below it).
pub fn rename_inside(cx: &mut gpui_kit::App, archive: PathBuf, inner: String, new_name: String) {
    let title = format!("{} {} → {new_name}", t!("Arşivde yeniden adlandırılıyor:"), inner.rsplit('/').next().unwrap_or_default());
    crate::status_center::start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let opts = options(&archive);
        let mut prog = crate::status_center::archive_progress_pub(&t);
        match rf_archive::rename(&archive, &inner, &new_name, &opts, &mut prog, &|| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

/// "New folder" inside an archive.
pub fn mkdir_inside(cx: &mut gpui_kit::App, archive: PathBuf, inner_dir: String, name: String) {
    let title = format!("{} {name}", t!("Arşivde klasör oluşturuluyor:"));
    crate::status_center::start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let opts = options(&archive);
        let mut prog = crate::status_center::archive_progress_pub(&t);
        match rf_archive::create_dir(&archive, &inner_dir, &name, &opts, &mut prog, &|| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

/// `name`, `name (2)`, … — the first one not among `taken` (case-insensitive).
pub fn unique_name(base: &str, ext: &str, taken: &[String]) -> String {
    let lower: Vec<String> = taken.iter().map(|t| t.to_lowercase()).collect();
    let mut n = 1;
    loop {
        let cand = if n == 1 { format!("{base}{ext}") } else { format!("{base} ({n}){ext}") };
        if !lower.contains(&cand.to_lowercase()) {
            return cand;
        }
        n += 1;
    }
}

/// Delete inside an archive (selection of an archive's folder view).
pub fn delete_inside(cx: &mut gpui_kit::App, archive: PathBuf, inners: Vec<String>) {
    let title = format!("{} {} ← {}", t!("Arşivden siliniyor:"), crate::i18n::items(inners.len()), rf_core::history::display_name(&archive));
    crate::status_center::start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let opts = options(&archive);
        let mut prog = crate::status_center::archive_progress_pub(&t);
        match rf_archive::delete(&archive, &inners, &opts, &mut prog, &|| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    });
}

/// Asks for an archive's password; `then` runs once one was entered.
pub fn ask_password(archive: PathBuf, wrong: bool, window: &mut gpui_kit::Window, cx: &mut gpui_kit::App, then: impl Fn(&mut gpui_kit::App) + 'static) {
    use crate::prompt::{Button, Kind};
    let msg = if wrong { t!("Parola yanlış. Tekrar deneyin.") } else { t!("Bu arşiv parola ile korunuyor.") };
    let name = rf_core::history::display_name(&archive);
    crate::prompt::show_secret(
        format!("{} — {name}", t!("Parola")),
        Some(msg.to_string()),
        t!("Parola").to_string(),
        vec![
            Button::new(t!("Tamam"), Kind::Primary, move |text, _, cx| {
                let pw = text.unwrap_or_default();
                if pw.is_empty() {
                    return Err(t!("Parola boş olamaz").to_string());
                }
                set_password(&archive, Some(pw));
                then(cx);
                Ok(())
            }),
            Button::cancel(),
        ],
        window,
        cx,
    );
}

/// Whether some component of `p` is named like an archive (no disk access).
pub fn is_browsable_path(p: &Path) -> bool {
    p.ancestors().any(is_browsable)
}

/// Splits a path into (archive file, path inside it) when some ancestor is an archive file.
/// A plain folder (or a non-existent path outside any archive) gives `None`.
pub fn split(path: &Path) -> Option<(PathBuf, PathBuf)> {
    if path.is_dir() {
        return None;
    }
    for a in path.ancestors() {
        if a.as_os_str().len() <= 3 {
            break;
        }
        if is_browsable(a) && a.is_file() {
            let inner = path.strip_prefix(a).ok()?.to_path_buf();
            return Some((a.to_path_buf(), inner));
        }
    }
    None
}

type Listing = Arc<Vec<rf_archive::ArchiveEntry>>;

static LISTINGS: Mutex<Option<HashMap<PathBuf, (i64, Listing)>>> = Mutex::new(None);

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn listing(archive: &Path) -> anyhow::Result<Listing> {
    let m = mtime_ms(archive);
    {
        let g = LISTINGS.lock().unwrap();
        if let Some((t, l)) = g.as_ref().and_then(|c| c.get(archive)) {
            if *t == m {
                return Ok(l.clone());
            }
        }
    }
    let l: Listing = Arc::new(rf_archive::list_with(archive, &options(archive))?);
    let mut g = LISTINGS.lock().unwrap();
    let c = g.get_or_insert_with(HashMap::new);
    if c.len() > 32 {
        c.clear();
    }
    c.insert(archive.to_path_buf(), (m, l.clone()));
    Ok(l)
}

/// The entries directly inside `inner` (folders implied by deeper paths included).
pub fn list_inside(archive: &Path, inner: &Path) -> anyhow::Result<Vec<Entry>> {
    let l = listing(archive)?;
    let prefix: String = inner.to_string_lossy().replace('\\', "/").trim_matches('/').to_string();
    let mut out: BTreeMap<String, Entry> = BTreeMap::new();
    for e in l.iter() {
        let p = e.path.replace('\\', "/");
        let p = p.trim_matches('/');
        let rest = if prefix.is_empty() {
            p
        } else if let Some(r) = p.strip_prefix(&prefix).and_then(|r| r.strip_prefix('/')) {
            r
        } else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            // a deeper entry: its first component is a folder here
            Some((dir, _)) => {
                out.entry(dir.to_lowercase()).or_insert_with(|| Entry::new(dir, EntryKind::Dir, 0, e.modified_ms, 0, 0x10));
            }
            None => {
                let kind = if e.is_dir { EntryKind::Dir } else { EntryKind::File };
                // read-only; encrypted members carry the Windows "encrypted" bit (a lock in the list)
                let attrs = if e.is_dir { 0x10 } else { 0x01 | if e.encrypted { rf_core::attrs::ENCRYPTED } else { 0 } };
                out.insert(rest.to_lowercase(), Entry::new(rest, kind, e.size, e.modified_ms, 0, attrs));
            }
        }
    }
    Ok(out.into_values().collect())
}

/// Where items opened or copied out of archives are extracted.
pub(crate) fn temp_root() -> PathBuf {
    std::env::temp_dir().join("smoothyfiles-arsiv")
}

/// Extracts the given items (paths inside archives) into a new temp folder per archive and
/// returns where each one landed, in order. Only the selected members are decoded; paths that
/// exist on disk pass through unchanged.
pub fn extract_items(paths: &[PathBuf], cancelled: &dyn Fn() -> bool, progress: &mut dyn FnMut(rf_archive::Progress)) -> anyhow::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = paths.to_vec();
    // archive → (indexes into `paths`, inner paths)
    let mut groups: Vec<(PathBuf, Vec<usize>, Vec<PathBuf>)> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if p.exists() {
            continue;
        }
        let Some((a, inner)) = split(p) else { continue };
        match groups.iter_mut().find(|g| g.0 == a) {
            Some(g) => {
                g.1.push(i);
                g.2.push(inner);
            }
            None => groups.push((a, vec![i], vec![inner])),
        }
    }
    static SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    for (archive, idx, inners) in groups {
        let stem = archive.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let n = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = temp_root().join(format!("{stem}-{stamp:x}{n}"));
        let mut opts = options(&archive);
        opts.only = inners.iter().map(|p| p.to_string_lossy().replace('\\', "/")).collect();
        if let Err(e) = rf_archive::extract_with(&archive, &dir, &opts, progress, cancelled) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        for (i, inner) in idx.into_iter().zip(inners) {
            let p = dir.join(&inner);
            if !p.exists() {
                let _ = std::fs::remove_dir_all(&dir);
                anyhow::bail!("{}: {}", t!("Arşivde bulunamadı"), inner.display());
            }
            out[i] = p;
        }
    }
    Ok(out)
}

/// Deletes extraction folders older than two days (an app may still have a newer one open).
pub fn clean_temp() {
    let Ok(rd) = std::fs::read_dir(temp_root()) else { return };
    let old = std::time::Duration::from_secs(2 * 24 * 3600);
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let age = m.created().or_else(|_| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if m.is_dir() && age.is_some_and(|a| a > old) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Opens items inside archives: extracts just them (Status Center shows progress and errors),
/// then opens each with its default app.
pub fn open_items(cx: &mut gpui_kit::App, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let title = format!("{} {}", t!("Arşivden açılıyor:"), names(&paths));
    crate::status_center::start(cx, title, move |t| {
        let cancel = t.cancel.clone();
        let mut prog = crate::status_center::archive_progress_pub(&t);
        let real = match extract_items(&paths, &|| cancel.load(std::sync::atomic::Ordering::Relaxed), &mut prog) {
            Ok(r) => r,
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => return Ok(false),
            Err(e) => return Err(e),
        };
        for p in real {
            rf_win::shell::open(&p)?;
        }
        Ok(true)
    });
}

/// Ctrl+C on items inside an archive: the clipboard is emptied at once (a quick paste must not
/// paste what was there before), the items are extracted, then the copies go on the clipboard
/// unless something else was copied meanwhile. Cut copies: the archive is left as it is.
pub fn copy_items(cx: &mut gpui_kit::App, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    rf_win::sta::run(move |owner| {
        if let Err(e) = rf_win::clipboard::clear(owner) {
            log::warn!("clipboard: {e:#}");
        }
        let _ = tx.send(rf_win::clipboard::sequence());
    });
    let title = format!("{} {}", t!("Arşivden kopyalanıyor:"), names(&paths));
    crate::status_center::start(cx, title, move |t| {
        let seq = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or(0);
        let cancel = t.cancel.clone();
        let mut prog = crate::status_center::archive_progress_pub(&t);
        let real = match extract_items(&paths, &|| cancel.load(std::sync::atomic::Ordering::Relaxed), &mut prog) {
            Ok(r) => r,
            Err(e) if e.downcast_ref::<rf_archive::Cancelled>().is_some() => return Ok(false),
            Err(e) => return Err(e),
        };
        if rf_win::clipboard::sequence() != seq {
            // the user copied something else in the meantime: keep that
            return Ok(true);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        rf_win::sta::run(move |owner| {
            let _ = tx.send(rf_win::clipboard::set_files(owner, &real, rf_win::fileops::Transfer::Copy));
        });
        rx.recv().map_err(|_| anyhow::anyhow!("pano"))??;
        Ok(true)
    });
}

/// Copies extracted for DirectLook: (archive, mtime, inner) → file. Moving through an archive
/// with the arrow keys must not extract the same member again and again.
static LOOK_COPIES: Mutex<Option<HashMap<(PathBuf, i64, PathBuf), PathBuf>>> = Mutex::new(None);

/// Members bigger than this are not extracted just because the selection passed over them.
const LOOK_FOLLOW_MAX: u64 = 64 << 20;

/// Sends a DirectLook message for `path`. An item inside an archive is extracted first (in the
/// background) and DirectLook gets the real file; `follow` (selection moved) skips big members.
pub fn look_send(path: PathBuf, size: u64, follow: bool) {
    use crate::look::ipc::{Msg, send};
    let msg = move |p: PathBuf| if follow { Msg::Follow(p) } else { Msg::Toggle(p) };
    if path.exists() || split(&path).is_none() {
        send(msg(path), !follow);
        return;
    }
    if follow && size > LOOK_FOLLOW_MAX {
        return;
    }
    std::thread::spawn(move || {
        if let Some(real) = real_copy(&path) {
            send(msg(real), !follow);
        }
    });
}

/// A real file for an item inside an archive: extracted once (blocking), then reused while the
/// archive is unchanged. `None` when it isn't in an archive or can't be extracted.
pub fn real_copy(path: &Path) -> Option<PathBuf> {
    let (archive, inner) = split(path)?;
    let key = (archive.clone(), mtime_ms(&archive), inner);
    if let Some(p) = LOOK_COPIES.lock().unwrap().as_ref().and_then(|m| m.get(&key).cloned()).filter(|p| p.exists()) {
        return Some(p);
    }
    match extract_items(&[path.to_path_buf()], &|| false, &mut |_| {}) {
        Ok(mut v) => {
            let p = v.pop()?;
            let mut g = LOOK_COPIES.lock().unwrap();
            let m = g.get_or_insert_with(HashMap::new);
            if m.len() > 256 {
                m.clear();
            }
            m.insert(key, p.clone());
            Some(p)
        }
        Err(e) => {
            log::warn!("archive item {path:?}: {e:#}");
            None
        }
    }
}

/// Members up to this size are extracted for the preview pane as the selection moves.
pub const PREVIEW_EXTRACT_MAX: u64 = LOOK_FOLLOW_MAX;

/// "a.txt" or "3 öğe" for a title.
fn names(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        _ => crate::i18n::items(paths.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_just_the_items_asked_for() {
        let base = std::env::temp_dir().join(format!("rf-av-items-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("proje");
        std::fs::create_dir_all(src.join("alt")).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("alt").join("b.txt"), b"bbb").unwrap();
        std::fs::write(src.join("buyuk.bin"), vec![7u8; 300_000]).unwrap();
        let zip = base.join("arsiv.zip");
        rf_archive::compress(&[src.clone()], &zip, rf_archive::Format::Zip, 5, &mut |_| {}, &|| false).unwrap();

        let outside = base.join("dis.txt");
        std::fs::write(&outside, b"x").unwrap();
        let asked = vec![zip.join("proje").join("alt").join("b.txt"), outside.clone(), zip.join("proje").join("a.txt")];
        let got = extract_items(&asked, &|| false, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(&got[0]).unwrap(), b"bbb");
        assert_eq!(got[1], outside); // real files pass through
        assert_eq!(std::fs::read(&got[2]).unwrap(), b"a");
        // one folder for the archive, without the member nobody asked for
        let root = got[0].parent().unwrap().parent().unwrap().parent().unwrap();
        assert_eq!(got[2].parent().unwrap().parent().unwrap(), root);
        assert!(!root.join("proje").join("buyuk.bin").exists());

        // asking again gives a fresh copy (the first may be open in an app)
        let again = extract_items(&asked[..1], &|| false, &mut |_| {}).unwrap();
        assert_ne!(again[0], got[0]);
        assert!(extract_items(&[zip.join("yok.txt")], &|| false, &mut |_| {}).is_err());

        for p in [root.to_path_buf(), again[0].parent().unwrap().parent().unwrap().parent().unwrap().to_path_buf()] {
            let _ = std::fs::remove_dir_all(p);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
