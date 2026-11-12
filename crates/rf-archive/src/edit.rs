//! Editing archives: [`add`] files / folders under an inner directory and [`delete`] entries.
//!
//! Every edit rewrites the archive into `<archive>.part` next to it and atomically renames that
//! over the original on success. Errors and cancellation remove the `.part` file and leave the
//! original untouched.
//!
//! * zip: untouched entries are raw-copied (no recompression, encrypted entries stay as they
//!   are, no password needed); new entries are deflated, AES-256 encrypted when the archive
//!   already holds encrypted entries and a password was given.
//! * 7z: the whole archive is decoded and re-encoded with LZMA2 (solid layout is not kept);
//!   an encrypted archive stays encrypted with the same password.
//! * tar, tar.gz, tar.zst, tar.xz: streamed through and re-compressed with the same codec.
//!
//! The password in [`Options`] is only used to *read* encrypted content and to keep an already
//! protected archive protected; an unprotected archive is never encrypted by an edit.

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};

use super::{
    Cancelled, CompressCtx, Item, Kind, Options, PasswordError, Progress, collect, kind_of, names,
    open_7z_archive, open_source, open_stream, sanitize, split_name, sz_content_methods, sz_is_encrypted,
    sz_password, sz_put, tar_put, volume, zip_put, zstd_level, CHUNK,
};

/// What an edit did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditStats {
    /// Files added.
    pub added_files: u64,
    /// Folders added (including the ones inside added folders).
    pub added_dirs: u64,
    /// Top-level added items that were auto-renamed (`name (2).ext`) because the name existed.
    pub renamed: u64,
    /// Archive entries removed (a deleted folder counts all of its entries).
    pub removed: u64,
    /// Size of the archive after the edit.
    pub archive_bytes: u64,
}

/// Whether [`add`] / [`delete`] can modify this archive: zip, 7z, tar, tar.gz, tar.zst, tar.xz
/// (not RAR, ISO, single-file gz / xz / zst, or split volumes).
pub fn is_editable(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if volume::split_number(&name).is_some() {
        return false;
    }
    matches!(kind_of(path), Some(Kind::Zip | Kind::SevenZ | Kind::Tar | Kind::TarGz | Kind::TarZst | Kind::TarXz | Kind::TarBz2))
}

/// Canonical form of an inner path: `/` separated, no empty / `.` components, no edge slashes.
fn norm(name: &str) -> String {
    name.replace('\\', "/").split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/")
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// Adds files / folders from disk under `inner_dir` (`/` separated path inside the archive,
/// `""` = root). A top-level name that already exists inside `inner_dir` becomes `name (2).ext`
/// (folders: `name (2)`); the archive itself is never added to itself. Symlinks / junctions are
/// skipped like in [`crate::compress`]. See the module docs for the per-format behaviour.
pub fn add(
    archive: &Path,
    sources: &[PathBuf],
    inner_dir: &str,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<EditStats> {
    ensure_editable(archive)?;
    if sources.is_empty() {
        bail!("Eklenecek öğe yok");
    }
    let inner = norm(inner_dir);
    if !inner.is_empty() {
        sanitize(&inner)?;
    }
    let part = part_path(archive);
    let mut items = collect(sources, &[archive.to_path_buf(), part])?;

    // Names that already exist in the archive (files, folders and the folders they imply).
    let mut taken: HashSet<String> = HashSet::new();
    for e in super::list_with(archive, opts)? {
        let n = norm(&e.path);
        let mut acc = String::new();
        for comp in n.split('/') {
            acc = join(&acc, comp);
            taken.insert(acc.to_lowercase());
        }
    }

    // Resolve top-level collisions, then move everything under `inner`.
    let mut tops: Vec<(String, String)> = Vec::new(); // (old top, new top)
    let mut renamed = 0u64;
    for it in &items {
        let top = it.name.split('/').next().unwrap_or("").to_string();
        if tops.iter().any(|(o, _)| *o == top) {
            continue;
        }
        let is_dir = it.is_dir && !it.name.contains('/');
        let mut new = top.clone();
        let mut n = 2;
        while taken.contains(&join(&inner, &new).to_lowercase()) {
            let (stem, ext) = if is_dir { (top.as_str(), "") } else { split_name(&top) };
            new = format!("{stem} ({n}){ext}");
            n += 1;
        }
        if new != top {
            renamed += 1;
        }
        taken.insert(join(&inner, &new).to_lowercase());
        tops.push((top, new));
    }
    for it in &mut items {
        let (top, rest) = match it.name.split_once('/') {
            Some((t, r)) => (t.to_string(), Some(r.to_string())),
            None => (it.name.clone(), None),
        };
        let new = &tops.iter().find(|(o, _)| *o == top).expect("top registered above").1;
        let name = match rest {
            Some(r) => format!("{new}/{r}"),
            None => new.clone(),
        };
        it.name = join(&inner, &name);
    }

    let added_files = items.iter().filter(|i| !i.is_dir).count() as u64;
    let added_dirs = items.iter().filter(|i| i.is_dir).count() as u64;
    let info = rewrite(archive, opts, &|_| false, &|_| None, items, progress, cancelled)?;
    Ok(EditStats { added_files, added_dirs, renamed, removed: info.removed, archive_bytes: info.archive_bytes })
}

/// Removes entries from the archive. A folder path removes the folder entry and everything
/// below it. Paths use `/` and match exactly (case-sensitive, like the stored names). If nothing
/// matches, the archive is left untouched and `removed` is 0.
pub fn delete(
    archive: &Path,
    inner_paths: &[String],
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<EditStats> {
    ensure_editable(archive)?;
    let paths: Vec<String> = inner_paths.iter().map(|p| norm(p)).filter(|p| !p.is_empty()).collect();
    if paths.is_empty() {
        bail!("Silinecek öğe yok");
    }
    let prefixes: Vec<String> = paths.iter().map(|p| format!("{p}/")).collect();
    let removed = |name: &str| {
        let n = norm(name);
        paths.iter().zip(&prefixes).any(|(p, pre)| n == *p || n.starts_with(pre.as_str()))
    };
    let info = rewrite(archive, opts, &removed, &|_| None, Vec::new(), progress, cancelled)?;
    Ok(EditStats { removed: info.removed, archive_bytes: info.archive_bytes, ..Default::default() })
}

/// Validates a single path component typed by the user (rename / new folder).
fn valid_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
    if bad {
        bail!("Geçersiz ad");
    }
    Ok(())
}

/// Every name that exists in the archive (lower-cased), including the folders entries imply.
fn existing_names(archive: &Path, opts: &Options) -> Result<(HashSet<String>, Vec<String>)> {
    let mut taken = HashSet::new();
    let mut raw = Vec::new();
    for e in super::list_with(archive, opts)? {
        let n = norm(&e.path);
        let mut acc = String::new();
        for comp in n.split('/') {
            acc = join(&acc, comp);
            taken.insert(acc.to_lowercase());
        }
        raw.push(n);
    }
    Ok((taken, raw))
}

/// Renames the file or folder `from` (`/` separated inner path) to `to_name` (new last
/// component only). A folder rename moves every entry below it. Errors: "Geçersiz ad",
/// "Öğe bulunamadı", "Bu adda bir öğe zaten var" (case-insensitive; a case-only change of the
/// same item is allowed). `EditStats::renamed` counts the renamed entries. Same formats and
/// encryption behaviour as [`add`] / [`delete`].
pub fn rename(
    archive: &Path,
    from: &str,
    to_name: &str,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<EditStats> {
    ensure_editable(archive)?;
    valid_name(to_name)?;
    let from = norm(from);
    if from.is_empty() {
        bail!("Öğe bulunamadı");
    }
    let prefix = format!("{from}/");
    let (taken, raw) = existing_names(archive, opts)?;
    if !raw.iter().any(|n| *n == from || n.starts_with(&prefix)) {
        bail!("Öğe bulunamadı");
    }
    let parent = from.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let new_from = join(parent, to_name);
    if new_from == from {
        return Ok(EditStats { archive_bytes: fs::metadata(archive).map(|m| m.len()).unwrap_or(0), ..Default::default() });
    }
    if new_from.to_lowercase() != from.to_lowercase() && taken.contains(&new_from.to_lowercase()) {
        bail!("Bu adda bir öğe zaten var");
    }
    let mapper = |name: &str| -> Option<String> {
        let n = norm(name);
        let slash = if name.ends_with('/') { "/" } else { "" };
        if n == from {
            Some(format!("{new_from}{slash}"))
        } else {
            n.strip_prefix(&prefix).map(|rest| format!("{new_from}/{rest}{slash}"))
        }
    };
    let info = rewrite(archive, opts, &|_| false, &mapper, Vec::new(), progress, cancelled)?;
    Ok(EditStats { renamed: info.mapped, archive_bytes: info.archive_bytes, ..Default::default() })
}

/// Adds an empty folder entry `inner_dir/name/`. Errors: "Geçersiz ad", "Bu adda bir öğe zaten
/// var" (case-insensitive). `EditStats::added_dirs` is 1 on success. Missing parent folders in
/// `inner_dir` are only implied (no extra entries are written).
pub fn create_dir(
    archive: &Path,
    inner_dir: &str,
    name: &str,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<EditStats> {
    ensure_editable(archive)?;
    valid_name(name)?;
    let inner = norm(inner_dir);
    if !inner.is_empty() {
        sanitize(&inner)?;
    }
    let full = join(&inner, name);
    let (taken, _) = existing_names(archive, opts)?;
    if taken.contains(&full.to_lowercase()) {
        bail!("Bu adda bir öğe zaten var");
    }
    // The shared writers take an on-disk source for the metadata; use a scratch directory.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "rf-archive-mkdir-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::create_dir_all(&scratch)?;
    let mtime = fs::metadata(&scratch).and_then(|m| m.modified()).ok();
    let item = Item { path: scratch.clone(), name: full, is_dir: true, size: 0, mtime };
    let res = rewrite(archive, opts, &|_| false, &|_| None, vec![item], progress, cancelled);
    let _ = fs::remove_dir(&scratch);
    let info = res?;
    Ok(EditStats { added_dirs: 1, archive_bytes: info.archive_bytes, ..Default::default() })
}

fn ensure_editable(archive: &Path) -> Result<()> {
    if !is_editable(archive) {
        bail!("Bu arşiv biçimi düzenlenemez");
    }
    Ok(())
}

fn part_path(archive: &Path) -> PathBuf {
    let mut s = archive.to_path_buf().into_os_string();
    s.push(".part");
    PathBuf::from(s)
}

struct Info {
    removed: u64,
    mapped: u64,
    archive_bytes: u64,
}

fn check(ctx: &mut CompressCtx) -> Result<()> {
    if (ctx.cancelled)() {
        ctx.was_cancelled = true;
        bail!("iptal");
    }
    Ok(())
}

/// Rewrites `archive` without the entries `removed` selects and with `items` appended.
fn rewrite(
    archive: &Path,
    opts: &Options,
    removed: &dyn Fn(&str) -> bool,
    mapper: &dyn Fn(&str) -> Option<String>,
    items: Vec<Item>,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<Info> {
    let kind = kind_of(archive).context("Desteklenmeyen arşiv türü")?;
    let part = part_path(archive);
    let _ = fs::remove_file(&part); // stale leftover of a crashed edit
    let new_total: u64 = items.iter().map(|i| i.size).sum();
    let mut ctx = CompressCtx {
        progress,
        cancelled,
        done: 0,
        total: new_total,
        last_emit: 0,
        was_cancelled: false,
        current: String::new(),
    };
    // Counts the entries `mapper` actually renamed.
    let mapped = Cell::new(0u64);
    let map = |n: &str| {
        let r = mapper(n);
        if r.is_some() {
            mapped.set(mapped.get() + 1);
        }
        r
    };
    let res = match kind {
        Kind::Zip => rewrite_zip(archive, &part, opts, removed, &map, &items, &mut ctx),
        Kind::SevenZ => rewrite_7z(archive, &part, opts, removed, &map, &items, &mut ctx),
        _ => rewrite_tar(kind, archive, &part, removed, &map, &items, &mut ctx),
    };
    match res {
        Ok(n) if n == 0 && items.is_empty() && mapped.get() == 0 => {
            let _ = fs::remove_file(&part);
            Ok(Info { removed: 0, mapped: 0, archive_bytes: fs::metadata(archive).map(|m| m.len()).unwrap_or(0) })
        }
        Ok(n) => {
            if let Err(e) = fs::rename(&part, archive) {
                let _ = fs::remove_file(&part);
                return Err(anyhow::Error::new(e).context(format!("Arşiv güncellenemedi: {}", archive.display())));
            }
            ctx.emit(true);
            Ok(Info { removed: n, mapped: mapped.get(), archive_bytes: fs::metadata(archive).map(|m| m.len()).unwrap_or(0) })
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            if ctx.was_cancelled || (ctx.cancelled)() { Err(Cancelled.into()) } else { Err(e) }
        }
    }
}

fn rewrite_zip(
    archive: &Path,
    part: &Path,
    opts: &Options,
    removed: &dyn Fn(&str) -> bool,
    map: &dyn Fn(&str) -> Option<String>,
    items: &[Item],
    ctx: &mut CompressCtx,
) -> Result<u64> {
    let mut za = zip::ZipArchive::new(BufReader::new(open_source(archive)?)).context("Zip dosyası okunamadı")?;
    let mut encrypted = false;
    let mut keep_bytes = 0u64;
    for i in 0..za.len() {
        let f = za.by_index_raw(i).context("Zip girdisi okunamadı")?;
        encrypted |= f.encrypted();
        let name = names::entry_name(f.name_raw(), f.name().context("Zip girdi adı çözülemedi")?).into_owned();
        if !removed(&name) {
            keep_bytes += f.compressed_size();
        }
    }
    ctx.total += keep_bytes;
    let pw = if encrypted { opts.pw() } else { None };
    let mut zw = zip::ZipWriter::new(BufWriter::with_capacity(CHUNK, File::create(part)?));
    let mut n_removed = 0u64;
    for i in 0..za.len() {
        check(ctx)?;
        let f = za.by_index_raw(i).context("Zip girdisi okunamadı")?;
        let name = names::entry_name(f.name_raw(), f.name().context("Zip girdi adı çözülemedi")?).into_owned();
        if removed(&name) {
            n_removed += 1;
            continue;
        }
        let csize = f.compressed_size();
        match map(&name) {
            Some(new) => zw.raw_copy_file_rename(f, new.as_str()).context("Zip girdisi kopyalanamadı")?,
            None => zw.raw_copy_file(f).context("Zip girdisi kopyalanamadı")?,
        }
        ctx.current = name;
        ctx.done += csize;
        ctx.emit(false);
    }
    for item in items {
        zip_put(&mut zw, item, 5, pw, ctx)?;
    }
    zw.finish()?.flush()?;
    Ok(n_removed)
}

/// Reader that forwards to the 7z entry stream while reporting progress / honouring cancel.
struct Pump<'a, 'b> {
    r: &'a mut dyn Read,
    ctx: &'a mut CompressCtx<'b>,
}

impl Read for Pump<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.ctx.cancelled)() {
            self.ctx.was_cancelled = true;
            return Err(io::Error::other("iptal"));
        }
        let n = self.r.read(buf)?;
        self.ctx.done += n as u64;
        self.ctx.emit(false);
        Ok(n)
    }
}

fn rewrite_7z(
    archive: &Path,
    part: &Path,
    opts: &Options,
    removed: &dyn Fn(&str) -> bool,
    map: &dyn Fn(&str) -> Option<String>,
    items: &[Item],
    ctx: &mut CompressCtx,
) -> Result<u64> {
    // Header-encrypted archives only open with the password; otherwise look at the coders.
    let (ar, header_encrypted) = match open_7z_archive(archive, &Options::default()) {
        Ok(a) => (a, false),
        Err(e) if e.downcast_ref::<PasswordError>() == Some(&PasswordError::Required) => {
            (open_7z_archive(archive, opts)?, true)
        }
        Err(e) => return Err(e),
    };
    let encrypted = header_encrypted || sz_is_encrypted(&ar);
    if encrypted && opts.pw().is_none() {
        return Err(PasswordError::Required.into());
    }
    let mut keep = 0u64;
    for e in &ar.files {
        if !e.is_anti_item && !e.is_directory && !removed(&e.name) {
            keep += e.size;
        }
    }
    ctx.total += keep;
    let mut reader = sevenz_rust2::ArchiveReader::from_archive(ar, open_source(archive)?, sz_password(opts));
    let mut sw = sevenz_rust2::ArchiveWriter::create(part).map_err(|e| anyhow::Error::new(e).context("7z oluşturulamadı"))?;
    sw.set_content_methods(sz_content_methods(5, if encrypted { opts.pw() } else { None }));

    let mut n_removed = 0u64;
    let mut failure: Option<anyhow::Error> = None;
    let res = reader.for_each_entries(|e, r| {
        if e.is_anti_item {
            return Ok(true);
        }
        if removed(&e.name) {
            n_removed += 1;
            return Ok(true);
        }
        if let Err(err) = check(ctx) {
            failure = Some(err);
            return Ok(false);
        }
        ctx.current = e.name.clone();
        let mut entry = e.clone();
        if let Some(new) = map(&e.name) {
            entry.name = new;
        }
        let pushed = if e.is_directory {
            sw.push_archive_entry::<&[u8]>(entry, None).map(|_| ())
        } else {
            sw.push_archive_entry(entry, Some(Pump { r, ctx: &mut *ctx })).map(|_| ())
        };
        match pushed {
            Ok(()) => Ok(true),
            Err(err) => {
                failure = Some(anyhow::Error::new(err).context(format!("7z'ye yazılamadı: {}", e.name)));
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    res.map_err(|e| super::map_7z_pw(e, opts))?;
    for item in items {
        sz_put(&mut sw, item, ctx)?;
    }
    sw.finish()?.flush()?;
    Ok(n_removed)
}

fn rewrite_tar(
    kind: Kind,
    archive: &Path,
    part: &Path,
    removed: &dyn Fn(&str) -> bool,
    map: &dyn Fn(&str) -> Option<String>,
    items: &[Item],
    ctx: &mut CompressCtx,
) -> Result<u64> {
    let old_len = super::source_len(archive)?;
    ctx.total += old_len;
    let counter = Rc::new(Cell::new(0u64));
    let mut ar = tar::Archive::new(open_stream(kind, archive, counter.clone())?);
    let out = BufWriter::with_capacity(CHUNK, File::create(part)?);
    let n: u64;
    let finish: Box<dyn FnOnce() -> io::Result<()>>;
    match kind {
        Kind::Tar => {
            let (k, out) = tar_copy(out, &mut ar, &counter, old_len, removed, map, items, ctx)?;
            n = k;
            finish = Box::new(move || out.into_inner().map_err(|e| e.into_error())?.flush());
        }
        Kind::TarGz => {
            let enc = flate2::write::GzEncoder::new(out, flate2::Compression::new(6));
            let (k, enc) = tar_copy(enc, &mut ar, &counter, old_len, removed, map, items, ctx)?;
            n = k;
            finish = Box::new(move || enc.finish()?.flush());
        }
        Kind::TarZst => {
            let enc = zstd::Encoder::new(out, zstd_level(5))?;
            let (k, enc) = tar_copy(enc, &mut ar, &counter, old_len, removed, map, items, ctx)?;
            n = k;
            finish = Box::new(move || enc.finish()?.flush());
        }
        Kind::TarXz => {
            let enc = lzma_rust2::XzWriter::new(out, lzma_rust2::XzOptions::with_preset(5))?;
            let (k, enc) = tar_copy(enc, &mut ar, &counter, old_len, removed, map, items, ctx)?;
            n = k;
            finish = Box::new(move || enc.finish()?.flush());
        }
        Kind::TarBz2 => {
            let enc = bzip2::write::BzEncoder::new(out, bzip2::Compression::new(6));
            let (k, enc) = tar_copy(enc, &mut ar, &counter, old_len, removed, map, items, ctx)?;
            n = k;
            finish = Box::new(move || enc.finish()?.flush());
        }
        _ => bail!("Bu arşiv biçimi düzenlenemez"),
    }
    finish().context("Arşiv yazılamadı")?;
    Ok(n)
}

/// Copies the old tar entries (minus the removed ones) into a new builder, then the new items.
fn tar_copy<W: Write, R: Read>(
    w: W,
    ar: &mut tar::Archive<R>,
    counter: &Rc<Cell<u64>>,
    old_len: u64,
    removed: &dyn Fn(&str) -> bool,
    map: &dyn Fn(&str) -> Option<String>,
    items: &[Item],
    ctx: &mut CompressCtx,
) -> Result<(u64, W)> {
    let mut tb = tar::Builder::new(w);
    let mut n_removed = 0u64;
    for e in ar.entries().context("Tar okunamadı")? {
        check(ctx)?;
        let mut e = e.context("Tar girdisi okunamadı")?;
        let t = e.header().entry_type();
        if !(t.is_file() || t.is_dir() || t.is_contiguous() || t.is_symlink() || t.is_hard_link()) {
            continue; // pax / GNU metadata records are regenerated by the builder
        }
        let name = String::from_utf8_lossy(&e.path_bytes()).into_owned();
        if removed(&name) {
            n_removed += 1;
            continue;
        }
        let new_name = map(&name).unwrap_or_else(|| name.clone());
        let mut h = e.header().clone();
        if t.is_symlink() || t.is_hard_link() {
            let target = e.link_name().context("Tar bağlantısı okunamadı")?.map(|l| l.into_owned());
            match target {
                Some(l) => tb.append_link(&mut h, &new_name, l)?,
                None => continue,
            }
        } else {
            tb.append_data(&mut h, &new_name, &mut e).with_context(|| format!("Kopyalanamadı: {name}"))?;
        }
        ctx.current = name;
        ctx.done = counter.get();
        ctx.emit(false);
    }
    ctx.done = old_len;
    for item in items {
        tar_put(&mut tb, item, ctx)?;
    }
    Ok((n_removed, tb.into_inner()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompressOptions, Format, compress_with, extract_with, list_with};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rf-edit2-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(p: Option<&str>) -> Options {
        Options { password: p.map(String::from), ..Default::default() }
    }

    /// proje/{a.txt, sub/b.txt, sub/deep/c.txt, empty/}
    fn build(format: Option<Format>, name: &str, pw: Option<&str>) -> (PathBuf, PathBuf) {
        let out = tmp("b");
        let root = out.join("proje");
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a.txt"), b"merhaba").unwrap();
        fs::write(root.join("sub/b.txt"), b"ikinci").unwrap();
        fs::write(root.join("sub/deep/c.txt"), b"ucuncu").unwrap();
        let arch = out.join(name);
        match format {
            Some(format) => {
                let co = CompressOptions { password: pw.map(String::from) };
                compress_with(&[root], &arch, format, 5, &co, &mut |_| {}, &|| false).unwrap();
            }
            None => {
                // tar.gz has no writer in the public API; build it directly
                let enc = flate2::write::GzEncoder::new(File::create(&arch).unwrap(), flate2::Compression::new(6));
                let mut tb = tar::Builder::new(enc);
                tb.append_dir_all("proje", &root).unwrap();
                tb.into_inner().unwrap().finish().unwrap();
            }
        }
        (out, arch)
    }

    fn names(arch: &Path, o: &Options) -> Vec<String> {
        let mut v: Vec<String> = list_with(arch, o).unwrap().iter().map(|e| norm(&e.path)).collect();
        v.sort();
        v
    }

    fn run_rename(arch: &Path, from: &str, to: &str, o: &Options) -> Result<EditStats> {
        rename(arch, from, to, o, &mut |_| {}, &|| false)
    }

    fn err(r: Result<EditStats>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    fn check_format(format: Option<Format>, name: &str) {
        let (out, arch) = build(format, name, None);
        let o = opts(None);
        // file
        let st = run_rename(&arch, "proje/a.txt", "z.txt", &o).unwrap();
        assert_eq!(st.renamed, 1);
        let n = names(&arch, &o);
        assert!(n.contains(&"proje/z.txt".to_string()) && !n.contains(&"proje/a.txt".to_string()), "{n:?}");
        // folder (with explicit dir entry and children)
        let st = run_rename(&arch, "proje/sub", "yeni", &o).unwrap();
        assert!(st.renamed >= 3, "{st:?}");
        let n = names(&arch, &o);
        assert!(n.contains(&"proje/yeni/deep/c.txt".to_string()), "{n:?}");
        assert!(!n.iter().any(|x| x.starts_with("proje/sub")), "{n:?}");
        // content intact after extraction
        let dest = out.join("x");
        extract_with(&arch, &dest, &o, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(dest.join("proje/yeni/deep/c.txt")).unwrap(), b"ucuncu");
        assert_eq!(fs::read(dest.join("proje/z.txt")).unwrap(), b"merhaba");
        // errors
        assert!(err(run_rename(&arch, "proje/z.txt", "YENI", &o)).contains("zaten var"));
        assert!(err(run_rename(&arch, "proje/yok.txt", "q", &o)).contains("Öğe bulunamadı"));
        for bad in ["", ".", "..", "a/b", r"a\b", "a:b", "a*", "a?", "a\"", "a<", "a>", "a|"] {
            assert!(err(run_rename(&arch, "proje/z.txt", bad, &o)).contains("Geçersiz ad"), "{bad}");
        }
        // case-only change is allowed
        run_rename(&arch, "proje/z.txt", "Z.txt", &o).unwrap();
        assert!(names(&arch, &o).contains(&"proje/Z.txt".to_string()));
        assert!(!part_path(&arch).exists());

        // create_dir
        create_dir(&arch, "proje", "klasor", &o, &mut |_| {}, &|| false).unwrap();
        create_dir(&arch, "proje/klasor", "ic", &o, &mut |_| {}, &|| false).unwrap();
        let n = names(&arch, &o);
        assert!(n.contains(&"proje/klasor".to_string()) && n.contains(&"proje/klasor/ic".to_string()), "{n:?}");
        let e = err(create_dir(&arch, "proje", "KLASOR", &o, &mut |_| {}, &|| false));
        assert!(e.contains("zaten var"), "{e}");
        let e = err(create_dir(&arch, "proje", "a/b", &o, &mut |_| {}, &|| false));
        assert!(e.contains("Geçersiz ad"), "{e}");
        let dest = out.join("y");
        extract_with(&arch, &dest, &o, &mut |_| {}, &|| false).unwrap();
        assert!(dest.join("proje/klasor/ic").is_dir());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn edit_zip() {
        check_format(Some(Format::Zip), "t.zip");
    }
    #[test]
    fn edit_7z() {
        check_format(Some(Format::SevenZ), "t.7z");
    }
    #[test]
    fn edit_tar_zst() {
        check_format(Some(Format::TarZst), "t.tar.zst");
    }
    #[test]
    fn edit_tar_gz() {
        check_format(None, "t.tar.gz");
    }

    fn check_encrypted(format: Option<Format>, name: &str) {
        let (out, arch) = build(format, name, Some("gizli"));
        let o = opts(Some("gizli"));
        run_rename(&arch, "proje/sub", "yeni", &o).unwrap();
        create_dir(&arch, "proje", "kl", &o, &mut |_| {}, &|| false).unwrap();
        assert!(crate::needs_password(&arch).unwrap());
        let dest = out.join("d");
        let e = extract_with(&arch, &dest, &opts(None), &mut |_| {}, &|| false).unwrap_err();
        assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Required), "{e:?}");
        extract_with(&arch, &dest, &o, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(dest.join("proje/yeni/b.txt")).unwrap(), b"ikinci");
        assert!(dest.join("proje/kl").is_dir());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rename_keeps_zip_encryption() {
        check_encrypted(Some(Format::Zip), "e.zip");
    }
    #[test]
    fn rename_keeps_7z_encryption() {
        check_encrypted(Some(Format::SevenZ), "e.7z");
    }

    #[test]
    fn cancel_keeps_original() {
        let (out, arch) = build(Some(Format::Zip), "c.zip", None);
        let before = fs::read(&arch).unwrap();
        let e = rename(&arch, "proje/a.txt", "q.txt", &opts(None), &mut |_| {}, &|| true).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
        assert_eq!(fs::read(&arch).unwrap(), before);
        assert!(!part_path(&arch).exists());
        let _ = fs::remove_dir_all(&out);
    }
}
