//! rf-archive: archive reading / extraction / creation / editing for the "Sıkıştır / Çıkar" actions
//! and for browsing archives like folders.
//!
//! Windows-only (ZIP timestamps go through the Windows time-zone APIs). Every function here is
//! blocking (disk + CPU): call off the UI thread. Rust / C dependencies: `zip`, `sevenz-rust2`,
//! `tar` + `flate2` / `zstd` / `lzma-rust2` / `bzip2` (libbz2-rs, pure Rust), our own `rarchive`
//! (RAR, and ZIP / 7z reading), `hadris-iso` / `hadris-udf` (pure Rust).
//!
//! | Format | List / extract | Create | Add / delete | Password |
//! |---|---|---|---|---|
//! | zip | yes | yes (deflate / store) | yes (raw copy of untouched entries) | read ZipCrypto + AES; write AES-256 |
//! | 7z | yes | yes (LZMA2 / copy) | yes (full re-encode) | read + write AES-256 (+ header encryption) |
//! | tar, tar.gz / tgz, tar.zst / tzst, tar.xz / txz, tar.bz2 / tbz2 | yes | tar.zst only | yes (same codec) | no |
//! | gz, xz, zst, bz2 (single file) | yes | no | no | no |
//! | rar 1.5 - 5 (incl. `.partN.rar`, `.r00` sets) | yes | no | no | read (RAR 3 - 5) |
//! | iso (ISO 9660 + Rock Ridge + Joliet, UDF 1.02 - 2.01) | yes | no | no | no |
//!
//! Split volumes (`name.7z.001`, `name.zip.001`, `name.tar.zst.001`, `name.001`, `name.iso.001`)
//! are read through one chained reader; give the first part (`.001`). The inner format comes from
//! the name before `.001` or from the magic bytes (zip / 7z). See [`volume_set`]. RAR sets are
//! followed by rarchive from any part's path.
//!
//! Known gaps:
//! * Creating RAR / ISO, creating tar.gz / tar.xz, creating split volumes.
//! * RAR 1.3 / 1.4, the RAR 1.5 / 2.0 encryption, a set with a part missing and RAR 7
//!   dictionaries over 4 GB are refused with a message saying why (nothing is written); RAR split
//!   as `x.rar.001` is not supported.
//! * UDF 2.50+ (metadata / virtual partitions: Blu-ray, rewritable media) and multi-session ISOs.
//! * Spanned zips in the `.z01` style (only 7-Zip style `.001` splits).
//! * Editing: 7z loses its solid layout, tar entries other than files / dirs / links are
//!   dropped, non-UTF-8 tar names are rewritten lossily; archives are rewritten as a whole.
//!
//! # Passwords
//! [`list_with`], [`extract_with`], [`add`], [`delete`] take [`Options`]. A missing / wrong
//! password surfaces as a [`PasswordError`] inside the `anyhow::Error` (`downcast_ref`).
//! [`needs_password`] is a cheap probe. [`compress_with`] encrypts zip / 7z; tar.zst refuses.
//!
//! # Extraction policy
//! * Entry names are validated before anything is written to disk (zip / 7z) or when reached
//!   (tar streams): absolute paths, drive prefixes (`C:`), `..` components and alternate data
//!   stream syntax (`:`) abort the whole extraction with an error (zip-slip protection).
//! * **Existing files are never overwritten**: a colliding file is auto-renamed to
//!   `name (2).ext`, `name (3).ext`, ... Directories merge into existing directories.
//! * Symbolic / hard links inside archives are skipped (counted in [`ExtractStats::skipped`]).
//! * File modification times are restored; directory times are not.

mod names;
mod timeconv;
mod edit;
mod iso;
mod own;
mod parwrite;
mod rar;
mod volume;
mod winrar;
mod zipwrite;

pub use edit::{EditStats, add, create_dir, delete, is_editable, rename};
pub use volume::volume_set;
pub use winrar::{compress_rar, rar_exe};
use volume::{open_source, source_len};

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

const CHUNK: usize = 256 * 1024;
const REPARSE_POINT: u32 = 0x400;

/// Returned (inside `anyhow::Error`, detect with `downcast_ref::<Cancelled>()`) when the
/// `cancelled` callback fired.
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("İptal edildi")
    }
}
impl std::error::Error for Cancelled {}

/// Returned (inside `anyhow::Error`, detect with `downcast_ref::<PasswordError>()`) when an
/// archive is encrypted and no password / a wrong password was supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordError {
    /// The archive is encrypted and no password was given.
    Required,
    /// A password was given but it does not decrypt the archive.
    Wrong,
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PasswordError::Required => "Bu arşiv parolalı",
            PasswordError::Wrong => "Parola yanlış",
        })
    }
}
impl std::error::Error for PasswordError {}

/// Options for reading ([`list_with`], [`extract_with`], [`add`], [`delete`]).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub password: Option<String>,
    /// Extraction only: paths inside the archive to write (a folder brings everything under
    /// it); empty = everything. Zip, 7z and RAR skip the other members without decoding them
    /// where the format allows; other formats read past them.
    pub only: Vec<String>,
}

/// The `Options::only` matcher.
#[derive(Debug, Clone, Default)]
pub(crate) struct Only(Vec<String>);

impl Only {
    pub(crate) fn new(list: &[String]) -> Only {
        Only(list.iter().map(|s| only_norm(s)).filter(|s| !s.is_empty()).collect())
    }

    pub(crate) fn wants(&self, rel: &Path) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let p = only_norm(&rel.to_string_lossy());
        self.0.iter().any(|s| p == *s || (p.len() > s.len() && p.starts_with(s.as_str()) && p.as_bytes()[s.len()] == b'/'))
    }
}

fn only_norm(s: &str) -> String {
    s.replace('\\', "/").trim_matches('/').to_lowercase()
}

impl Options {
    /// The password, `None` when absent or empty.
    fn pw(&self) -> Option<&str> {
        self.password.as_deref().filter(|p| !p.is_empty())
    }
}

/// Options for creating archives ([`compress_with`]).
#[derive(Debug, Clone, Default)]
pub struct CompressOptions {
    /// Encrypts the archive: AES-256 for zip, AES-256 with header encryption for 7z.
    pub password: Option<String>,
}

/// One archive member as shown in the preview list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Name inside the archive, `/` separated, as stored (not sanitized).
    pub path: String,
    /// Uncompressed size (0 for directories).
    pub size: u64,
    /// Compressed size when the format records it per entry (zip, 7z); 0 = unknown (tar family).
    pub packed: u64,
    pub is_dir: bool,
    /// Unix milliseconds, 0 when unknown.
    pub modified_ms: i64,
    /// The member's content is encrypted (a password is needed to read it).
    pub encrypted: bool,
}

/// Progress report. For zip / 7z the unit is uncompressed bytes. For tar-family streams and
/// single-file gz / xz / zst the unit is *compressed* bytes consumed from the archive file
/// (the uncompressed total is not known without a full pass).
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Entry currently being processed (archive-relative name).
    pub current: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Files that were auto-renamed because the name already existed.
    pub renamed: u64,
    /// Links / unsupported entry types that were skipped.
    pub skipped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    SevenZ,
    TarZst,
}

impl Format {
    /// Conventional file extension (without dot).
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::TarZst => "tar.zst",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressStats {
    /// Final archive path (may differ from the requested one if that name was taken).
    pub path: PathBuf,
    pub files: u64,
    pub dirs: u64,
    /// Total uncompressed bytes added.
    pub bytes: u64,
    pub archive_bytes: u64,
}

// ---------------------------------------------------------------- format detection

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    SevenZ,
    Tar,
    TarGz,
    TarZst,
    TarXz,
    TarBz2,
    Gz,
    Xz,
    Zst,
    Bz2,
    Rar,
    Iso,
}

/// (suffix, kind) — longest suffixes first.
const SUFFIXES: &[(&str, Kind)] = &[
    (".tar.gz", Kind::TarGz),
    (".tar.zst", Kind::TarZst),
    (".tar.xz", Kind::TarXz),
    (".tar.bz2", Kind::TarBz2),
    (".tbz2", Kind::TarBz2),
    (".tbz", Kind::TarBz2),
    (".tgz", Kind::TarGz),
    (".tzst", Kind::TarZst),
    (".txz", Kind::TarXz),
    (".zip", Kind::Zip),
    (".rar", Kind::Rar),
    (".iso", Kind::Iso),
    (".7z", Kind::SevenZ),
    (".tar", Kind::Tar),
    (".gz", Kind::Gz),
    (".xz", Kind::Xz),
    (".zst", Kind::Zst),
    (".bz2", Kind::Bz2),
    // comic book archives
    (".cbz", Kind::Zip),
    (".cbr", Kind::Rar),
    (".cb7", Kind::SevenZ),
    (".cbt", Kind::Tar),
];

/// Extensions (without dot) that [`list`] / [`extract`] can read.
pub fn formats() -> &'static [&'static str] {
    &["zip", "7z", "rar", "iso", "tar", "tar.gz", "tgz", "tar.zst", "tzst", "tar.xz", "txz", "tar.bz2", "tbz2", "tbz", "gz", "xz", "zst", "bz2", "001", "cbz", "cbr", "cb7", "cbt"]
}

/// Whether `path` has an extension we can read.
pub fn is_supported(path: &Path) -> bool {
    kind_of(path).is_some()
}

fn file_name_lower(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn kind_of_name(n: &str) -> Option<Kind> {
    if let Some(k) = SUFFIXES.iter().find(|(s, _)| n.len() > s.len() && n.ends_with(s)).map(|(_, k)| *k) {
        return Some(k);
    }
    // Old-style RAR continuation volumes: x.r00, x.r01, ...
    let ext = n.rsplit_once('.').map(|(_, e)| e)?;
    let d = ext.strip_prefix('r')?;
    (d.len() >= 2 && d.bytes().all(|b| b.is_ascii_digit())).then_some(Kind::Rar)
}

/// Format of `path`: by (lowercase) name suffix; for a split set (`x.7z.001`, `x.001`) by the name
/// before `.001`, falling back to the magic bytes of the first part. Only the first part counts.
fn kind_of(path: &Path) -> Option<Kind> {
    let n = file_name_lower(path);
    if let Some((base, num)) = volume::split_number(&n) {
        if num != 1 {
            return None;
        }
        return kind_of_name(base).or_else(|| sniff_kind(path));
    }
    // Comic book archives are often misnamed (a zip called .cbr): trust the content.
    if [".cbz", ".cbr", ".cb7"].iter().any(|s| n.ends_with(s)) {
        if let Some(k) = sniff_kind(path) {
            return Some(k);
        }
    }
    kind_of_name(&n)
}

/// Detects zip / 7z / rar by magic bytes.
fn sniff_kind(path: &Path) -> Option<Kind> {
    let mut f = File::open(path).ok()?;
    let mut b = [0u8; 6];
    f.read_exact(&mut b).ok()?;
    if &b == b"7z\xBC\xAF\x27\x1C" {
        Some(Kind::SevenZ)
    } else if b[0] == b'P' && b[1] == b'K' && matches!((b[2], b[3]), (3, 4) | (5, 6)) {
        Some(Kind::Zip)
    } else if &b == b"Rar!\x1a\x07" {
        Some(Kind::Rar)
    } else {
        None
    }
}

/// Splits `a.tar.gz` -> (`a`, `.tar.gz`); falls back to the last extension.
fn split_name(name: &str) -> (&str, &str) {
    if let Some((base, _)) = volume::split_number(name) {
        let (stem, _) = split_name(base);
        return (stem, &name[stem.len()..]);
    }
    // x.part1.rar -> stem "x"
    let lower_name = name.to_ascii_lowercase();
    if let Some(stem) = lower_name.strip_suffix(".rar") {
        if let Some(i) = stem.rfind(".part") {
            let digits = &stem[i + 5..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return (&name[..i], &name[i..]);
            }
        }
    }
    let lower = name.to_lowercase();
    for (s, _) in SUFFIXES {
        if lower.len() > s.len() && lower.ends_with(s) {
            // lowercasing can change byte lengths (İ): only cut on a real char boundary
            let cut = name.len().saturating_sub(s.len());
            if let (Some(a), Some(b)) = (name.get(..cut), name.get(cut..)) {
                return (a, b);
            }
        }
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// First free `dir\name`, `dir\name (2)`, ... If `keep_ext`, the number goes before the
/// extension (`name (2).ext`, known archive suffixes such as `.tar.gz` count as one extension).
fn unique_path(p: &Path, keep_ext: bool) -> PathBuf {
    if fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = if keep_ext { split_name(&name) } else { (name.as_str(), "") };
    let dir = p.parent().unwrap_or(Path::new(""));
    for n in 2..100_000u32 {
        let cand = dir.join(format!("{stem} ({n}){ext}"));
        if fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
    }
    p.to_path_buf()
}

/// Files' "Extract to <name>\" target: a folder next to the archive, named after it without the
/// archive extension (`photos.tar.gz` -> `photos`), made unique (`photos (2)`) if it exists.
pub fn smart_extract_dir(archive: &Path) -> PathBuf {
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, _) = split_name(&name);
    let stem = if stem.is_empty() { "Arşiv" } else { stem };
    let dir = archive.parent().unwrap_or(Path::new("")).join(stem);
    unique_path(&dir, false)
}

// ---------------------------------------------------------------- name sanitizing

/// A device name of Windows (CON, NUL, COM1… with or without an extension) can't be a file:
/// written as is it vanishes (NUL) or fails. Such a member is kept as `NUL_.txt`.
fn windows_safe(comp: &str) -> String {
    let base = comp.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((base.starts_with("COM") || base.starts_with("LPT")) && base.len() == 4 && matches!(base.as_bytes()[3], b'1'..=b'9'))
        || ((base.starts_with("COM") || base.starts_with("LPT")) && ["¹", "²", "³"].iter().any(|s| base[3..] == **s));
    if !reserved {
        return comp.to_string();
    }
    match comp.find('.') {
        Some(i) => format!("{}_{}", &comp[..i], &comp[i..]),
        None => format!("{comp}_"),
    }
}

/// Validates an archive member name and turns it into a relative path. `Ok(None)` for names
/// that denote the archive root (`./`, empty). Errors on anything that could escape the target.
fn sanitize(name: &str) -> Result<Option<PathBuf>> {
    let n = name.replace('\\', "/");
    if n.starts_with('/') {
        bail!("Güvensiz arşiv girdisi (mutlak yol): {name}");
    }
    let mut out = PathBuf::new();
    for comp in n.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." || comp.contains(':') || comp.contains('\0') {
            bail!("Güvensiz arşiv girdisi (yol geçişi): {name}");
        }
        // Win32 strips trailing dots/spaces, so ".. " would resolve to "..".
        if comp.trim_end_matches(['.', ' ']).is_empty() {
            bail!("Güvensiz arşiv girdisi (geçersiz ad): {name}");
        }
        out.push(windows_safe(comp));
    }
    Ok(if out.as_os_str().is_empty() { None } else { Some(out) })
}

// ---------------------------------------------------------------- readers

struct Counting<R> {
    inner: R,
    n: Rc<Cell<u64>>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n.set(self.n.get() + k as u64);
        Ok(k)
    }
}

/// Opens the (decompressed) byte stream of a tar / single-file archive.
fn open_stream(kind: Kind, path: &Path, counter: Rc<Cell<u64>>) -> Result<Box<dyn Read>> {
    let f = open_source(path).with_context(|| format!("Arşiv açılamadı: {}", path.display()))?;
    let raw = BufReader::with_capacity(CHUNK, Counting { inner: f, n: counter });
    Ok(match kind {
        Kind::Tar => Box::new(raw),
        Kind::TarGz | Kind::Gz => Box::new(BufReader::with_capacity(CHUNK, flate2::read::MultiGzDecoder::new(raw))),
        Kind::TarZst | Kind::Zst => Box::new(BufReader::with_capacity(CHUNK, zstd::Decoder::with_buffer(raw)?)),
        Kind::TarXz | Kind::Xz => Box::new(BufReader::with_capacity(CHUNK, lzma_rust2::XzReader::new(raw, true))),
        Kind::TarBz2 | Kind::Bz2 => Box::new(BufReader::with_capacity(CHUNK, bzip2::read::MultiBzDecoder::new(raw))),
        Kind::Zip | Kind::SevenZ | Kind::Rar | Kind::Iso => unreachable!("not a stream format"),
    })
}

fn is_tar(kind: Kind) -> bool {
    matches!(kind, Kind::Tar | Kind::TarGz | Kind::TarZst | Kind::TarXz | Kind::TarBz2)
}

fn mtime_ms(t: SystemTime) -> i64 {
    timeconv::unix_ms(t)
}

fn zip_mtime(dt: Option<zip::DateTime>) -> Option<SystemTime> {
    let d = dt?;
    timeconv::dos_local_to_systemtime(d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second())
}

fn nt_mtime(e: &sevenz_rust2::ArchiveEntry) -> Option<SystemTime> {
    if !e.has_last_modified_date {
        return None;
    }
    let ticks: u64 = e.last_modified_date.into();
    (ticks > 0).then(|| SystemTime::from(e.last_modified_date))
}

fn tar_mtime(secs: u64) -> Option<SystemTime> {
    (secs > 0).then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

fn sz_password(opts: &Options) -> sevenz_rust2::Password {
    match opts.pw() {
        Some(p) => sevenz_rust2::Password::new(p),
        None => sevenz_rust2::Password::empty(),
    }
}

/// Reads the 7z header. Header-encrypted archives need the password already here.
fn open_7z_archive(path: &Path, opts: &Options) -> Result<sevenz_rust2::Archive> {
    let mut src = BufReader::new(open_source(path).with_context(|| format!("Arşiv açılamadı: {}", path.display()))?);
    sevenz_rust2::Archive::read(&mut src, &sz_password(opts)).map_err(|e| map_7z_pw(e, opts))
}

/// Whether any block of the archive uses AES (content encrypted).
fn sz_is_encrypted(ar: &sevenz_rust2::Archive) -> bool {
    ar.blocks.iter().any(|b| b.coders.iter().any(|c| c.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256))
}

/// Maps a 7z error; password-related failures become [`PasswordError`].
fn map_7z_pw(e: sevenz_rust2::Error, opts: &Options) -> anyhow::Error {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired => PasswordError::Required.into(),
        E::MaybeBadPassword(_) => if opts.pw().is_some() { PasswordError::Wrong } else { PasswordError::Required }.into(),
        // A wrong password on header-encrypted archives shows up as garbage in the header.
        E::NextHeaderCrcMismatch | E::ChecksumVerificationFailed if opts.pw().is_some() => PasswordError::Wrong.into(),
        e => map_7z(e),
    }
}

// ---------------------------------------------------------------- list

/// Lists the archive contents for a preview. zip / 7z read only the central directory; tar
/// streams must be decompressed end to end (can be slow for huge archives).
pub fn list(path: &Path) -> Result<Vec<ArchiveEntry>> {
    list_with(path, &Options::default())
}

/// [`list`] with options (password). Zip names are never encrypted, so a zip lists without a
/// password; a 7z with encrypted headers fails with [`PasswordError`] when none / a wrong one is given.
pub fn list_with(path: &Path, opts: &Options) -> Result<Vec<ArchiveEntry>> {
    let kind = kind_of(path).with_context(|| format!("Desteklenmeyen arşiv türü: {}", path.display()))?;
    match kind {
        Kind::Zip if rarchive::Archive::open(path).is_ok() => {
            // our own reader lists anything with a readable directory (split sets too)
            let a = rarchive::Archive::open(path).map_err(own::map_err)?;
            Ok(a.entries()
                .iter()
                .map(|e| ArchiveEntry {
                    path: own::display_name(e),
                    size: e.size,
                    packed: e.packed_size,
                    is_dir: e.is_dir,
                    modified_ms: own::mtime(e).map_or(0, mtime_ms),
                    encrypted: e.encrypted,
                })
                .collect())
        }
        Kind::Zip => {
            let mut za = zip::ZipArchive::new(BufReader::new(open_source(path)?)).context("Zip dosyası okunamadı")?;
            let mut out = Vec::with_capacity(za.len());
            for i in 0..za.len() {
                let f = za.by_index_raw(i).context("Zip girdisi okunamadı")?;
                out.push(ArchiveEntry {
                    path: f.name().map(|n| names::entry_name(f.name_raw(), n).into_owned()).unwrap_or_default(),
                    size: f.size(),
                    packed: f.compressed_size(),
                    is_dir: f.is_dir(),
                    modified_ms: zip_mtime(f.last_modified()).map_or(0, mtime_ms),
                    encrypted: f.encrypted(),
                });
            }
            Ok(out)
        }
        Kind::SevenZ if own_archive(path, opts)?.is_some() => Ok(own_entries(&own_archive(path, opts)?.unwrap())),
        Kind::SevenZ => {
            let ar = open_7z_archive(path, opts)?;
            Ok(ar
                .files
                .iter()
                .filter(|e| !e.is_anti_item)
                .map(|e| ArchiveEntry {
                    path: e.name.clone(),
                    size: if e.is_directory { 0 } else { e.size },
                    packed: e.compressed_size,
                    is_dir: e.is_directory,
                    modified_ms: nt_mtime(e).map_or(0, mtime_ms),
                    encrypted: false,
                })
                .collect())
        }
        Kind::Rar => rar::list(path, opts),
        Kind::Iso => iso::list(path, opts),
        k if is_tar(k) => {
            let mut out = Vec::new();
            let counter = Rc::new(Cell::new(0));
            let stream = open_stream(k, path, counter)?;
            let mut ar = tar::Archive::new(stream);
            for e in ar.entries().context("Tar okunamadı")? {
                let e = e.context("Tar girdisi okunamadı")?;
                let h = e.header();
                let t = h.entry_type();
                if !(t.is_file() || t.is_dir() || t.is_contiguous()) {
                    continue;
                }
                out.push(ArchiveEntry {
                    path: String::from_utf8_lossy(&e.path_bytes()).into_owned(),
                    size: if t.is_dir() { 0 } else { h.size().unwrap_or(0) },
                    packed: 0,
                    is_dir: t.is_dir(),
                    modified_ms: h.mtime().ok().and_then(tar_mtime).map_or(0, mtime_ms),
                    encrypted: false,
                });
            }
            Ok(out)
        }
        k => {
            // Single compressed file.
            let meta = fs::metadata(path)?;
            let packed = source_len(path)?;
            let size = match k {
                Kind::Gz => gzip_isize(path).unwrap_or(0),
                _ => {
                    let mut s = open_stream(k, path, Rc::new(Cell::new(0)))?;
                    io::copy(&mut s, &mut io::sink()).context("Sıkıştırılmış dosya okunamadı")?
                }
            };
            Ok(vec![ArchiveEntry {
                path: single_name(path),
                size,
                packed,
                is_dir: false,
                modified_ms: meta.modified().map_or(0, mtime_ms),
                encrypted: false,
            }])
        }
    }
}

/// Cheap probe: does reading this archive's content need a password? Zip: any encrypted entry
/// (central directory only); 7z: encrypted header or an AES coder. Other formats: `false`.
pub fn needs_password(path: &Path) -> Result<bool> {
    let kind = kind_of(path).with_context(|| format!("Desteklenmeyen arşiv türü: {}", path.display()))?;
    match kind {
        Kind::Zip if own_zip(path, &Options::default()).is_some() => Ok(own_zip(path, &Options::default()).unwrap().needs_password()),
        Kind::Zip => {
            let mut za = zip::ZipArchive::new(BufReader::new(open_source(path)?)).context("Zip dosyası okunamadı")?;
            for i in 0..za.len() {
                if za.by_index_raw(i).context("Zip girdisi okunamadı")?.encrypted() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Kind::SevenZ if matches!(rarchive::Archive::open(path), Err(rarchive::Error::NeedPassword)) => Ok(true),
        Kind::SevenZ if own_archive(path, &Options::default()).ok().flatten().is_some() => {
            Ok(own_archive(path, &Options::default())?.unwrap().needs_password())
        }
        Kind::SevenZ => match open_7z_archive(path, &Options::default()) {
            Ok(ar) => Ok(sz_is_encrypted(&ar)),
            Err(e) if e.downcast_ref::<PasswordError>().is_some() => Ok(true),
            Err(e) => Err(e),
        },
        Kind::Rar => rar::needs_password(path),
        _ => Ok(false),
    }
}

/// Checks a password before anything is extracted: `Err(PasswordError::Wrong)` when it does
/// not open the archive. ZIP names are never encrypted, so a listing alone can't tell; this
/// decrypts the start of the first protected entry instead (AES has a verifier, ZipCrypto a
/// check byte — the rare false positive surfaces later as a CRC error mapped to `Wrong`).
pub fn check_password(path: &Path, opts: &Options) -> Result<()> {
    let kind = kind_of(path).with_context(|| format!("Desteklenmeyen arşiv türü: {}", path.display()))?;
    match kind {
        Kind::Zip if own_zip(path, opts).is_some() => {
            own_zip(path, opts).unwrap().verify_password().map_err(own::map_err)
        }
        Kind::Zip => {
            let Some(pw) = opts.password.as_deref() else { return Err(PasswordError::Required.into()) };
            let mut za = zip::ZipArchive::new(BufReader::new(open_source(path)?)).context("Zip dosyası okunamadı")?;
            for i in 0..za.len() {
                if !za.by_index_raw(i).context("Zip girdisi okunamadı")?.encrypted() {
                    continue;
                }
                let mut f = match za.by_index_decrypt(i, pw.as_bytes()) {
                    Ok(f) => f,
                    Err(zip::result::ZipError::InvalidPassword) => return Err(PasswordError::Wrong.into()),
                    Err(e) => return Err(e).context("Zip girdisi okunamadı"),
                };
                let mut buf = [0u8; 4096];
                return match f.read(&mut buf) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(PasswordError::Wrong.into()),
                };
            }
            Ok(())
        }
        Kind::Rar => rar::check_password(path, opts),
        Kind::SevenZ if own_archive(path, opts)?.is_some() => own_archive(path, opts)?.unwrap().verify_password().map_err(own::map_err),
        _ => list_with(path, opts).map(|_| ()),
    }
}

fn gzip_isize(path: &Path) -> Option<u64> {
    let mut f = open_source(path).ok()?;
    f.seek(SeekFrom::End(-4)).ok()?;
    let mut b = [0u8; 4];
    f.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b) as u64)
}

fn single_name(path: &Path) -> String {
    let n = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, _) = split_name(&n);
    if stem.is_empty() { "çıktı".to_string() } else { stem.to_string() }
}

fn map_7z(e: sevenz_rust2::Error) -> anyhow::Error {
    anyhow::Error::new(e).context("7z arşivi okunamadı")
}

// ---------------------------------------------------------------- extract

struct Extractor<'a> {
    dest: &'a Path,
    progress: &'a mut dyn FnMut(Progress),
    cancelled: &'a dyn Fn() -> bool,
    total: u64,
    done: u64,
    /// When set, progress is measured in compressed bytes read from the archive file.
    packed: Option<Rc<Cell<u64>>>,
    stats: ExtractStats,
    buf: Vec<u8>,
    last_emit: u64,
    /// Set when reading from the archive (not writing to disk) failed.
    read_failed: bool,
    only: Only,
}

impl<'a> Extractor<'a> {
    fn new(
        dest: &'a Path,
        total: u64,
        packed: Option<Rc<Cell<u64>>>,
        progress: &'a mut dyn FnMut(Progress),
        cancelled: &'a dyn Fn() -> bool,
    ) -> Self {
        Extractor {
            dest,
            progress,
            cancelled,
            total,
            done: 0,
            packed,
            stats: ExtractStats::default(),
            buf: vec![0u8; CHUNK],
            last_emit: 0,
            read_failed: false,
            only: Only::default(),
        }
    }

    fn only(mut self, only: Only) -> Self {
        self.only = only;
        self
    }

    fn emit(&mut self, current: &str, force: bool) {
        let done = self.packed.as_ref().map_or(self.done, |c| c.get());
        if force || done.saturating_sub(self.last_emit) >= 128 * 1024 || done >= self.total {
            self.last_emit = done;
            (self.progress)(Progress { done_bytes: done, total_bytes: self.total, current: current.to_string() });
        }
    }

    fn check_cancel(&self) -> Result<()> {
        if (self.cancelled)() { Err(Cancelled.into()) } else { Ok(()) }
    }

    fn dir(&mut self, rel: &Path, display: &str) -> Result<()> {
        self.check_cancel()?;
        if !self.only.wants(rel) {
            return Ok(());
        }
        let target = self.dest.join(rel);
        fs::create_dir_all(&target).with_context(|| format!("Klasör oluşturulamadı: {}", target.display()))?;
        self.stats.dirs += 1;
        self.emit(display, false);
        Ok(())
    }

    fn file(&mut self, rel: &Path, display: &str, r: &mut dyn Read, mtime: Option<SystemTime>) -> Result<()> {
        self.check_cancel()?;
        if !self.only.wants(rel) {
            // read past it: some readers (solid 7z) expect each member to be consumed
            io::copy(r, &mut io::sink()).with_context(|| format!("Arşiv girdisi okunamadı: {display}"))?;
            return Ok(());
        }
        let wanted = self.dest.join(rel);
        if let Some(parent) = wanted.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Klasör oluşturulamadı: {}", parent.display()))?;
        }
        // Never overwrite: pick a free name; create_new closes the race window.
        let mut target = unique_path(&wanted, true);
        let mut file = loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&target) {
                Ok(f) => break f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => target = unique_path(&wanted, true),
                Err(e) => return Err(e).with_context(|| format!("Dosya oluşturulamadı: {}", target.display())),
            }
        };
        if target != wanted {
            self.stats.renamed += 1;
        }
        self.emit(display, true);
        let res = (|| -> Result<u64> {
            let mut w = BufWriter::with_capacity(CHUNK, &mut file);
            let mut n = 0u64;
            loop {
                self.check_cancel()?;
                let k = match r.read(&mut self.buf) {
                    Ok(k) => k,
                    Err(e) => {
                        self.read_failed = true;
                        return Err(anyhow::Error::new(e).context(format!("Arşiv girdisi okunamadı: {display}")));
                    }
                };
                if k == 0 {
                    break;
                }
                w.write_all(&self.buf[..k]).with_context(|| format!("Yazılamadı: {}", target.display()))?;
                n += k as u64;
                self.done += k as u64;
                self.emit(display, false);
            }
            w.flush()?;
            drop(w);
            if let Some(t) = mtime {
                let _ = file.set_modified(t);
            }
            Ok(n)
        })();
        drop(file);
        match res {
            Ok(n) => {
                self.stats.files += 1;
                self.stats.bytes += n;
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_file(&target); // partial file
                Err(e)
            }
        }
    }
}

/// Extracts `archive` into `dest_dir` (created if missing). See the module docs for the
/// path-safety and existing-file policy. `progress` is invoked from the calling thread,
/// roughly every 128 KiB; `cancelled` is polled between chunks (partial file is removed and
/// an error downcastable to [`Cancelled`] is returned).
pub fn extract(
    archive: &Path,
    dest_dir: &Path,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    extract_with(archive, dest_dir, &Options::default(), progress, cancelled)
}

/// [`extract`] with options (password). Encrypted archives without / with a wrong password fail
/// with a [`PasswordError`] (the first encrypted entry is probed up front, so normally nothing
/// is written).
pub fn extract_with(
    archive: &Path,
    dest_dir: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    let kind = kind_of(archive).with_context(|| format!("Desteklenmeyen arşiv türü: {}", archive.display()))?;
    fs::create_dir_all(dest_dir).with_context(|| format!("Klasör oluşturulamadı: {}", dest_dir.display()))?;
    match kind {
        Kind::Zip => extract_zip(archive, dest_dir, opts, progress, cancelled),
        Kind::SevenZ => extract_7z(archive, dest_dir, opts, progress, cancelled),
        Kind::Rar => rar::extract(archive, dest_dir, opts, progress, cancelled),
        Kind::Iso => iso::extract(archive, dest_dir, opts, progress, cancelled),
        k if is_tar(k) => extract_tar(k, archive, dest_dir, opts, progress, cancelled),
        k => extract_single(k, archive, dest_dir, progress, cancelled),
    }
}

fn extract_zip(
    archive: &Path,
    dest: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    // Our own decoder when it can do the whole archive: parallel, low priority.
    if let Some(a) = own_zip(archive, opts) {
        if a.needs_password() {
            a.verify_password().map_err(own::map_err)?;
        }
        return own::extract(a, dest, &Only::new(&opts.only), progress, cancelled);
    }
    let mut za = zip::ZipArchive::new(BufReader::new(open_source(archive)?)).context("Zip dosyası okunamadı")?;
    // Validate every name first so a hostile archive writes nothing at all.
    let mut total = 0u64;
    let mut first_encrypted = None;
    for i in 0..za.len() {
        let f = za.by_index_raw(i).context("Zip girdisi okunamadı")?;
        sanitize(&names::entry_name(f.name_raw(), f.name().context("Zip girdi adı çözülemedi")?))?;
        if f.encrypted() && !f.is_dir() && first_encrypted.is_none() {
            first_encrypted = Some(i);
        }
        total += f.size();
    }
    let pw = opts.pw().map(|p| p.as_bytes());
    if let Some(i) = first_encrypted {
        // Up-front password probe: opening the entry verifies the password check value.
        let Some(pw) = pw else { return Err(PasswordError::Required.into()) };
        zip_open(&mut za, i, Some(pw))?;
    }
    let mut ex = Extractor::new(dest, total, None, progress, cancelled).only(Only::new(&opts.only));
    for i in 0..za.len() {
        let mut f = zip_open(&mut za, i, pw)?;
        let name = names::entry_name(f.name_raw(), f.name().context("Zip girdi adı çözülemedi")?).into_owned();
        let Some(rel) = sanitize(&name)? else { continue };
        let encrypted = f.encrypted();
        let r = if f.is_symlink() {
            ex.stats.skipped += 1;
            Ok(())
        } else if f.is_dir() {
            ex.dir(&rel, &name)
        } else {
            let mt = zip_mtime(f.last_modified());
            ex.file(&rel, &name, &mut f, mt)
        };
        if let Err(e) = r {
            // ZipCrypto's check byte lets 1 in 256 wrong passwords through: the CRC then fails.
            return Err(if encrypted && ex.read_failed { PasswordError::Wrong.into() } else { e });
        }
    }
    ex.emit("", true);
    Ok(ex.stats)
}

/// The archive through our own decoder when it can do all of it; password problems come
/// back as [`PasswordError`] (archives with encrypted names need the password to open).
fn own_archive(path: &Path, opts: &Options) -> Result<Option<rarchive::Archive>> {
    match rarchive::Archive::open_with_password(path, opts.pw()) {
        Ok(a) if a.can_extract() => Ok(Some(a)),
        Err(e @ (rarchive::Error::NeedPassword | rarchive::Error::BadPassword)) => Err(own::map_err(e)),
        _ => Ok(None),
    }
}

/// An archive's members as listed by our own reader.
fn own_entries(a: &rarchive::Archive) -> Vec<ArchiveEntry> {
    a.entries()
        .iter()
        .map(|e| ArchiveEntry {
            path: own::display_name(e),
            size: e.size,
            packed: e.packed_size,
            is_dir: e.is_dir,
            modified_ms: own::mtime(e).map_or(0, mtime_ms),
            encrypted: e.encrypted,
        })
        .collect()
}

/// The ZIP through our own decoder, when it can do all of it (split and spanned sets
/// included; unsupported methods / encryption go through the zip crate).
fn own_zip(path: &Path, opts: &Options) -> Option<rarchive::Archive> {
    rarchive::Archive::open_with_password(path, opts.pw()).ok().filter(|a| a.can_extract())
}

/// Opens one zip entry, decrypting when needed; password problems become [`PasswordError`].
fn zip_open<'a, R: Read + Seek>(
    za: &'a mut zip::ZipArchive<R>,
    i: usize,
    pw: Option<&[u8]>,
) -> Result<zip::read::ZipFile<'a, R>> {
    let res = match pw {
        Some(p) => za.by_index_decrypt(i, p),
        None => za.by_index(i),
    };
    res.map_err(|e| match e {
        zip::result::ZipError::InvalidPassword => PasswordError::Wrong.into(),
        zip::result::ZipError::UnsupportedArchive(m) if m == zip::result::ZipError::PASSWORD_REQUIRED => {
            PasswordError::Required.into()
        }
        e => anyhow::Error::new(e).context("Zip girdisi okunamadı"),
    })
}

fn extract_7z(
    archive: &Path,
    dest: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    if let Some(a) = own_archive(archive, opts)? {
        if a.needs_password() {
            a.verify_password().map_err(own::map_err)?;
        }
        return own::extract(a, dest, &Only::new(&opts.only), progress, cancelled);
    }
    let ar = open_7z_archive(archive, opts)?;
    let encrypted = sz_is_encrypted(&ar);
    if encrypted && opts.pw().is_none() {
        return Err(PasswordError::Required.into());
    }
    let mut reader = sevenz_rust2::ArchiveReader::from_archive(ar, open_source(archive)?, sz_password(opts));
    let mut total = 0u64;
    for e in &reader.archive().files {
        sanitize(&e.name)?;
        if !e.is_directory {
            total += e.size;
        }
    }
    let mut ex = Extractor::new(dest, total, None, progress, cancelled).only(Only::new(&opts.only));
    let mut failure: Option<anyhow::Error> = None;
    let res = reader.for_each_entries(|e, r| {
        let mut step = || -> Result<()> {
            if e.is_anti_item {
                return Ok(());
            }
            let Some(rel) = sanitize(&e.name)? else { return Ok(()) };
            if e.has_windows_attributes && e.windows_attributes & REPARSE_POINT != 0 {
                ex.stats.skipped += 1;
            } else if e.is_directory {
                ex.dir(&rel, &e.name)?;
            } else {
                ex.file(&rel, &e.name, r, nt_mtime(e))?;
            }
            Ok(())
        };
        match step() {
            Ok(()) => Ok(true),
            Err(err) => {
                failure = Some(err);
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(if encrypted && ex.read_failed { PasswordError::Wrong.into() } else { e });
    }
    res.map_err(|e| map_7z_pw(e, opts))?;
    ex.emit("", true);
    Ok(ex.stats)
}

fn extract_tar(
    kind: Kind,
    archive: &Path,
    dest: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    let total = source_len(archive)?;
    let counter = Rc::new(Cell::new(0u64));
    let mut ex = Extractor::new(dest, total, Some(counter.clone()), progress, cancelled).only(Only::new(&opts.only));
    let mut ar = tar::Archive::new(open_stream(kind, archive, counter)?);
    // small files are written by a pool while the stream keeps decoding (see parwrite)
    let pw = parwrite::ParWriter::new(parwrite::threads());
    let res = (|| -> Result<()> {
        for e in ar.entries().context("Tar okunamadı")? {
            let mut e = e.context("Tar girdisi okunamadı")?;
            pw.check()?;
            let name = String::from_utf8_lossy(&e.path_bytes()).into_owned();
            let t = e.header().entry_type();
            let mt = e.header().mtime().ok().and_then(tar_mtime);
            if t.is_dir() {
                if let Some(rel) = sanitize(&name)? {
                    ex.dir(&rel, &name)?;
                }
            } else if t.is_file() || t.is_contiguous() {
                let Some(rel) = sanitize(&name)? else { continue };
                let size = e.header().size().unwrap_or(u64::MAX);
                if !ex.only.wants(&rel) {
                    // not selected: the tar reader skips its data
                    continue;
                }
                if size <= parwrite::SMALL {
                    ex.check_cancel()?;
                    let mut data = Vec::with_capacity(size as usize);
                    if let Err(err) = e.read_to_end(&mut data) {
                        ex.read_failed = true;
                        return Err(anyhow::Error::new(err).context(format!("Arşiv girdisi okunamadı: {name}")));
                    }
                    ex.emit(&name, false);
                    pw.submit(dest.join(&rel), data, mt)?;
                } else {
                    ex.file(&rel, &name, &mut e, mt)?;
                }
            } else if t.is_symlink() || t.is_hard_link() {
                sanitize(&name)?;
                ex.stats.skipped += 1;
            }
            // pax / GNU metadata entries are consumed by the tar crate itself.
        }
        Ok(())
    })();
    if res.is_err() {
        pw.abandon();
        drop(pw);
        return res.map(|_| ex.stats);
    }
    let (files, bytes, renamed) = pw.finish()?;
    ex.stats.files += files;
    ex.stats.bytes += bytes;
    ex.stats.renamed += renamed;
    ex.emit("", true);
    Ok(ex.stats)
}

fn extract_single(
    kind: Kind,
    archive: &Path,
    dest: &Path,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    let meta = fs::metadata(archive)?;
    let counter = Rc::new(Cell::new(0u64));
    let mut ex = Extractor::new(dest, source_len(archive)?, Some(counter.clone()), progress, cancelled);
    let mut s = open_stream(kind, archive, counter)?;
    let name = single_name(archive);
    let rel = sanitize(&name)?.context("Geçersiz dosya adı")?;
    ex.file(&rel, &name, &mut s, meta.modified().ok())?;
    ex.emit("", true);
    Ok(ex.stats)
}

// ---------------------------------------------------------------- compress

struct Item {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
}

fn is_reparse(m: &fs::Metadata) -> bool {
    m.file_attributes() & REPARSE_POINT != 0
}

fn collect(sources: &[PathBuf], skip: &[PathBuf]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let skip_names: Vec<_> = skip.iter().filter_map(|p| p.file_name().map(|n| n.to_os_string())).collect();
    let skip_canon: Vec<PathBuf> = skip
        .iter()
        .filter_map(|p| Some(fs::canonicalize(p.parent()?).ok()?.join(p.file_name()?)))
        .collect();
    let is_skipped = |p: &Path| -> bool {
        let Some(n) = p.file_name() else { return false };
        if !skip_names.iter().any(|s| s.eq_ignore_ascii_case(n)) {
            return false;
        }
        fs::canonicalize(p).map(|c| skip_canon.iter().any(|s| s.as_os_str().eq_ignore_ascii_case(c.as_os_str()))).unwrap_or(false)
    };

    fn walk(dir: &Path, prefix: &str, items: &mut Vec<Item>, is_skipped: &dyn Fn(&Path) -> bool) -> Result<()> {
        let mut children: Vec<_> = fs::read_dir(dir)
            .with_context(|| format!("Klasör okunamadı: {}", dir.display()))?
            .filter_map(|e| e.ok())
            .collect();
        children.sort_by_key(|e| e.file_name());
        for c in children {
            let p = c.path();
            let Ok(m) = fs::symlink_metadata(&p) else { continue };
            if is_reparse(&m) || is_skipped(&p) {
                continue;
            }
            let name = format!("{prefix}/{}", c.file_name().to_string_lossy());
            if m.is_dir() {
                items.push(Item { path: p.clone(), name: name.clone(), is_dir: true, size: 0, mtime: m.modified().ok() });
                walk(&p, &name, items, is_skipped)?;
            } else if m.is_file() {
                items.push(Item { path: p, name, is_dir: false, size: m.len(), mtime: m.modified().ok() });
            }
        }
        Ok(())
    }

    for src in sources {
        let m = fs::symlink_metadata(src).with_context(|| format!("Kaynak bulunamadı: {}", src.display()))?;
        if is_reparse(&m) || is_skipped(src) {
            continue;
        }
        let base = src.file_name().map(|n| n.to_string_lossy().into_owned()).context("Geçersiz kaynak yolu")?;
        // Unique top-level names so two sources called "a" do not collide inside the archive.
        let mut name = base.clone();
        let mut n = 2;
        while !used.insert(name.to_lowercase()) {
            let (stem, ext) = split_name(&base);
            name = format!("{stem} ({n}){ext}");
            n += 1;
        }
        if m.is_dir() {
            items.push(Item { path: src.clone(), name: name.clone(), is_dir: true, size: 0, mtime: m.modified().ok() });
            walk(src, &name, &mut items, &is_skipped)?;
        } else if m.is_file() {
            items.push(Item { path: src.clone(), name, is_dir: false, size: m.len(), mtime: m.modified().ok() });
        }
    }
    Ok(items)
}

struct CompressCtx<'a> {
    progress: &'a mut dyn FnMut(Progress),
    cancelled: &'a dyn Fn() -> bool,
    done: u64,
    total: u64,
    last_emit: u64,
    was_cancelled: bool,
    current: String,
}

impl CompressCtx<'_> {
    fn emit(&mut self, force: bool) {
        if force || self.done.saturating_sub(self.last_emit) >= 128 * 1024 || self.done >= self.total {
            self.last_emit = self.done;
            (self.progress)(Progress { done_bytes: self.done, total_bytes: self.total, current: self.current.clone() });
        }
    }
}

/// File reader that reports progress and honours cancellation.
struct Feed<'a, 'b> {
    file: File,
    ctx: &'a mut CompressCtx<'b>,
}

impl Read for Feed<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.ctx.cancelled)() {
            self.ctx.was_cancelled = true;
            return Err(io::Error::other("iptal"));
        }
        let n = self.file.read(buf)?;
        self.ctx.done += n as u64;
        self.ctx.emit(false);
        Ok(n)
    }
}

fn zstd_level(level: u32) -> i32 {
    [1, 3, 5, 7, 9, 11, 13, 15, 17, 19][level.min(9) as usize]
}

/// Creates `dest_archive` from files / folders. `level` is 0 (store / fastest) to 9 (smallest);
/// 5 is a good default. Folders are added recursively under their own name; symlinks and
/// junctions are skipped. If `dest_archive` already exists a free name (`x (2).zip`) is used —
/// see [`CompressStats::path`]. The archive is written to `<name>.part` and renamed on success,
/// so cancellation or failure never leaves a truncated archive behind.
pub fn compress(
    sources: &[PathBuf],
    dest_archive: &Path,
    format: Format,
    level: u32,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<CompressStats> {
    compress_with(sources, dest_archive, format, level, &CompressOptions::default(), progress, cancelled)
}

/// [`compress`] with options. With a password zip uses AES-256 and 7z uses AES-256 plus
/// header encryption (file names hidden); tar.zst cannot be encrypted and fails with
/// "Bu biçim parolayı desteklemiyor".
pub fn compress_with(
    sources: &[PathBuf],
    dest_archive: &Path,
    format: Format,
    level: u32,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<CompressStats> {
    if sources.is_empty() {
        bail!("Sıkıştırılacak öğe yok");
    }
    let password = opts.password.as_deref().filter(|p| !p.is_empty());
    if password.is_some() && format == Format::TarZst {
        bail!("Bu biçim parolayı desteklemiyor");
    }
    let level = level.min(9);
    let final_path = unique_path(dest_archive, true);
    let part = {
        let mut s = final_path.clone().into_os_string();
        s.push(".part");
        PathBuf::from(s)
    };
    if let Some(parent) = final_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let items = collect(sources, &[final_path.clone(), part.clone()])?;
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut ctx = CompressCtx { progress, cancelled, done: 0, total, last_emit: 0, was_cancelled: false, current: String::new() };

    let res = match format {
        Format::Zip => write_zip(&items, &part, level, password, &mut ctx),
        Format::SevenZ => write_7z(&items, &part, level, password, &mut ctx),
        Format::TarZst => write_tar_zst(&items, &part, level, &mut ctx),
    };
    match res {
        Ok(()) => {
            fs::rename(&part, &final_path).with_context(|| format!("Arşiv kaydedilemedi: {}", final_path.display()))?;
            ctx.emit(true);
            Ok(CompressStats {
                path: final_path.clone(),
                files: items.iter().filter(|i| !i.is_dir).count() as u64,
                dirs: items.iter().filter(|i| i.is_dir).count() as u64,
                bytes: total,
                archive_bytes: fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0),
            })
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            if ctx.was_cancelled || (ctx.cancelled)() { Err(Cancelled.into()) } else { Err(e) }
        }
    }
}

fn feed<'a, 'b>(item: &Item, ctx: &'a mut CompressCtx<'b>) -> Result<Feed<'a, 'b>> {
    ctx.current = item.name.clone();
    ctx.emit(true);
    let file = File::open(&item.path).with_context(|| format!("Dosya açılamadı: {}", item.path.display()))?;
    Ok(Feed { file, ctx })
}

fn zip_put<W: Write + Seek>(
    zw: &mut zip::ZipWriter<W>,
    item: &Item,
    level: u32,
    password: Option<&str>,
    ctx: &mut CompressCtx,
) -> Result<()> {
    use zip::write::SimpleFileOptions;
    if (ctx.cancelled)() {
        ctx.was_cancelled = true;
        bail!("iptal");
    }
    let method = if level == 0 { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
    let mut opts = SimpleFileOptions::default().compression_method(method);
    if level > 0 {
        opts = opts.compression_level(Some(level as i64));
    }
    if let Some(dt) = item.mtime.and_then(timeconv::systemtime_to_local_fields).and_then(|(y, mo, d, h, mi, s)| {
        zip::DateTime::from_date_and_time(y, mo, d, h, mi, s).ok()
    }) {
        opts = opts.last_modified_time(dt);
    }
    if item.is_dir {
        zw.add_directory(item.name.clone(), opts)?;
    } else {
        opts = opts.large_file(item.size >= 0xFFFF_0000);
        if let Some(pw) = password {
            let opts = opts.with_aes_encryption(zip::AesMode::Aes256, pw);
            zw.start_file(item.name.clone(), opts)?;
        } else {
            zw.start_file(item.name.clone(), opts)?;
        }
        let src = feed(item, ctx)?;
        io::copy(&mut BufReader::with_capacity(CHUNK, src), zw)
            .with_context(|| format!("Eklenemedi: {}", item.path.display()))?;
    }
    Ok(())
}

fn write_zip(items: &[Item], part: &Path, level: u32, password: Option<&str>, ctx: &mut CompressCtx) -> Result<()> {
    // our parallel writer; a password needs WinZip AES from the zip crate
    if password.is_none() {
        return zipwrite::write(items, part, level, ctx, false);
    }
    let mut zw = zip::ZipWriter::new(BufWriter::with_capacity(CHUNK, File::create(part)?));
    for item in items {
        zip_put(&mut zw, item, level, password, ctx)?;
    }
    zw.finish()?.flush()?;
    Ok(())
}

fn sz_content_methods(level: u32, password: Option<&str>) -> Vec<sevenz_rust2::EncoderConfiguration> {
    sz_content_methods_mt(level, password, false)
}

/// `mt`: LZMA2 in independent chunks on several threads (for big solid blocks).
fn sz_content_methods_mt(level: u32, password: Option<&str>, mt: bool) -> Vec<sevenz_rust2::EncoderConfiguration> {
    use sevenz_rust2::{EncoderConfiguration, EncoderMethod, encoder_options::*};
    let cfg = if level == 0 {
        EncoderConfiguration::new(EncoderMethod::COPY)
    } else {
        let opts = match sz_threads(level) {
            (t, chunk) if mt && t > 1 => Lzma2Options::from_level_mt(level, t, chunk),
            _ => Lzma2Options::from_level(level),
        };
        EncoderConfiguration::new(EncoderMethod::LZMA2).with_options(EncoderOptions::Lzma2(opts))
    };
    match password {
        // AES first, then the compressor (the order the sevenz-rust2 docs use); the header is
        // encrypted too (default), so names are hidden.
        Some(pw) => vec![AesEncoderOptions::new(pw.into()).into(), cfg],
        None => vec![cfg],
    }
}

fn sz_put<W: Write + Seek>(sw: &mut sevenz_rust2::ArchiveWriter<W>, item: &Item, ctx: &mut CompressCtx) -> Result<()> {
    use sevenz_rust2::ArchiveEntry;
    if (ctx.cancelled)() {
        ctx.was_cancelled = true;
        bail!("iptal");
    }
    let entry = ArchiveEntry::from_path(&item.path, item.name.clone());
    if item.is_dir {
        sw.push_archive_entry::<&[u8]>(entry, None).map_err(|e| anyhow::Error::new(e).context("7z'ye eklenemedi"))?;
    } else {
        let src = feed(item, ctx)?;
        sw.push_archive_entry(entry, Some(src))
            .map_err(|e| anyhow::Error::new(e).context(format!("7z'ye eklenemedi: {}", item.path.display())))?;
    }
    Ok(())
}

/// A source file opened on its first read and closed at its end (a solid block takes the
/// readers of all its files up front: thousands of open handles otherwise). Reports
/// progress and honours cancellation from the calling thread.
struct LazySource<'a, 'b> {
    path: PathBuf,
    file: Option<File>,
    finished: bool,
    ctx: &'a std::cell::RefCell<&'a mut CompressCtx<'b>>,
}

impl Read for LazySource<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished {
            return Ok(0);
        }
        {
            let mut c = self.ctx.borrow_mut();
            if (c.cancelled)() {
                c.was_cancelled = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "iptal"));
            }
        }
        if self.file.is_none() {
            let f = File::open(&self.path).map_err(|e| io::Error::new(e.kind(), format!("Dosya açılamadı: {}: {e}", self.path.display())))?;
            self.file = Some(f);
            self.ctx.borrow_mut().current = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        }
        let n = self.file.as_mut().unwrap().read(buf)?;
        if n == 0 {
            self.file = None;
            self.finished = true;
        }
        let mut c = self.ctx.borrow_mut();
        c.done += n as u64;
        c.emit(false);
        Ok(n)
    }
}

/// LZMA2 threads and chunk size: 7-Zip-style, each chunk (4 × the dictionary, 32 MB at
/// least) is compressed on its own thread. The encoder needs ~11 × the dictionary per thread,
/// so the thread count keeps that near 1 GB.
fn sz_threads(level: u32) -> (u32, u64) {
    let dict: u64 = [1 << 18, 1 << 20, 1 << 21, 1 << 22, 1 << 22, 1 << 23, 1 << 23, 1 << 24, 1 << 25, 1 << 26][level.min(9) as usize];
    let chunk = (dict * 4).max(32 << 20);
    let per_thread = dict * 11 + chunk;
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let threads = cores.min((1u64 << 30) / per_thread).clamp(1, 32);
    (threads as u32, chunk)
}

/// Files per solid block at most (bytes): one damaged block or one file to extract never
/// means decoding everything.
const SOLID_BLOCK: u64 = 1 << 30;

fn write_7z(items: &[Item], part: &Path, level: u32, password: Option<&str>, ctx: &mut CompressCtx) -> Result<()> {
    use sevenz_rust2::{ArchiveEntry, SourceReader};
    let mut sw = sevenz_rust2::ArchiveWriter::create(part).map_err(|e| anyhow::Error::new(e).context("7z oluşturulamadı"))?;
    sw.set_content_methods(sz_content_methods_mt(level, password, true));
    // folders and empty files carry no data
    for item in items.iter().filter(|i| i.is_dir || i.size == 0) {
        if (ctx.cancelled)() {
            ctx.was_cancelled = true;
            bail!("iptal");
        }
        let entry = ArchiveEntry::from_path(&item.path, item.name.clone());
        sw.push_archive_entry::<&[u8]>(entry, None).map_err(|e| anyhow::Error::new(e).context("7z'ye eklenemedi"))?;
    }
    // the rest solid (like 7-Zip), in blocks; LZMA2 compresses each block on several threads
    let files: Vec<&Item> = items.iter().filter(|i| !i.is_dir && i.size > 0).collect();
    let cell = std::cell::RefCell::new(ctx);
    let mut start = 0;
    while start < files.len() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < files.len() && (end == start || bytes + files[end].size <= SOLID_BLOCK) {
            bytes += files[end].size;
            end += 1;
        }
        let block = &files[start..end];
        let entries: Vec<ArchiveEntry> = block.iter().map(|i| ArchiveEntry::from_path(&i.path, i.name.clone())).collect();
        let readers: Vec<SourceReader<LazySource>> =
            block.iter().map(|i| SourceReader::new(LazySource { path: i.path.clone(), file: None, finished: false, ctx: &cell })).collect();
        let r = sw.push_archive_entries(entries, readers);
        if cell.borrow().was_cancelled {
            bail!("iptal");
        }
        r.map_err(|e| anyhow::Error::new(e).context("7z'ye eklenemedi"))?;
        start = end;
    }
    sw.finish()?.flush()?;
    Ok(())
}

fn tar_put<W: Write>(tb: &mut tar::Builder<W>, item: &Item, ctx: &mut CompressCtx) -> Result<()> {
    if (ctx.cancelled)() {
        ctx.was_cancelled = true;
        bail!("iptal");
    }
    let meta = fs::metadata(&item.path).with_context(|| format!("Okunamadı: {}", item.path.display()))?;
    let mut h = tar::Header::new_gnu();
    h.set_metadata(&meta);
    if item.is_dir {
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        tb.append_data(&mut h, format!("{}/", item.name), io::empty())?;
    } else {
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(item.size);
        let src = feed(item, ctx)?;
        tb.append_data(&mut h, &item.name, BufReader::with_capacity(CHUNK, src))
            .with_context(|| format!("Eklenemedi: {}", item.path.display()))?;
    }
    Ok(())
}

fn write_tar_zst(items: &[Item], part: &Path, level: u32, ctx: &mut CompressCtx) -> Result<()> {
    let mut enc = zstd::Encoder::new(BufWriter::with_capacity(CHUNK, File::create(part)?), zstd_level(level))?;
    let _ = enc.long_distance_matching(level >= 7);
    // zstd's own worker threads (it compresses jobs of ~a few MB in parallel)
    let _ = enc.multithread(std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16) as u32);
    let mut tb = tar::Builder::new(enc);
    for item in items {
        tar_put(&mut tb, item, ctx)?;
    }
    let enc = tb.into_inner()?;
    enc.finish()?.flush()?;
    Ok(())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_keep_their_data() {
        assert_eq!(windows_safe("NUL"), "NUL_");
        assert_eq!(windows_safe("con.txt"), "con_.txt");
        assert_eq!(windows_safe("COM1.tar.gz"), "COM1_.tar.gz");
        assert_eq!(windows_safe("Console.txt"), "Console.txt");
        assert_eq!(windows_safe("COM10"), "COM10");
        assert_eq!(sanitize("a/AUX/b.txt").unwrap(), Some(PathBuf::from("a/AUX_/b.txt")));
    }
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rf-archive-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// root/{a.txt, big.bin, sub/b.txt, sub/deep/c.txt, empty/}
    fn make_tree() -> PathBuf {
        let root = tmp("src").join("proje");
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a.txt"), b"merhaba dunya").unwrap();
        fs::write(root.join("sub/b.txt"), b"ikinci").unwrap();
        fs::write(root.join("sub/deep/c.txt"), "çğüşiö unicode içerik").unwrap();
        let big: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(root.join("big.bin"), big).unwrap();
        root
    }

    fn assert_same(a: &Path, b: &Path) {
        for e in fs::read_dir(a).unwrap() {
            let e = e.unwrap();
            let other = b.join(e.file_name());
            assert!(other.exists(), "eksik: {}", other.display());
            if e.path().is_dir() {
                assert_same(&e.path(), &other);
            } else {
                assert_eq!(fs::read(e.path()).unwrap(), fs::read(&other).unwrap(), "{}", other.display());
            }
        }
    }

    #[test]
    fn extracts_only_the_selected_entries() {
        for (format, name) in [(Format::Zip, "s.zip"), (Format::SevenZ, "s.7z"), (Format::TarZst, "s.tar.zst")] {
            let root = make_tree();
            let out = tmp("only");
            let arch = out.join(name);
            compress(&[root.clone()], &arch, format, 5, &mut |_| {}, &|| false).unwrap();

            // one file (case and separators don't matter)
            let dest = out.join("tek");
            let opts = Options { only: vec![r"Proje\SUB\b.txt".into()], ..Default::default() };
            let st = extract_with(&arch, &dest, &opts, &mut |_| {}, &|| false).unwrap();
            assert_eq!(st.files, 1, "{name}");
            assert_eq!(fs::read(dest.join("proje/sub/b.txt")).unwrap(), b"ikinci");
            assert!(!dest.join("proje/a.txt").exists() && !dest.join("proje/big.bin").exists(), "{name}");

            // a folder brings everything under it, nothing else
            let dest = out.join("klasor");
            let opts = Options { only: vec!["proje/sub".into()], ..Default::default() };
            let st = extract_with(&arch, &dest, &opts, &mut |_| {}, &|| false).unwrap();
            assert_eq!(st.files, 2, "{name}");
            assert_same(&root.join("sub"), &dest.join("proje/sub"));
            assert!(!dest.join("proje/a.txt").exists() && !dest.join("proje/empty").exists(), "{name}");
        }
    }

    fn roundtrip(format: Format, name: &str) {
        let root = make_tree();
        let out = tmp("out");
        let arch = out.join(name);
        let mut last = Progress::default();
        let st = compress(&[root.clone()], &arch, format, 5, &mut |p| last = p, &|| false).unwrap();
        assert_eq!(st.path, arch);
        assert_eq!(st.files, 4);
        assert!(st.dirs >= 4); // proje, sub, deep, empty
        assert!(arch.exists() && !out.join(format!("{name}.part")).exists());
        assert_eq!(last.done_bytes, last.total_bytes);

        let listed = list(&arch).unwrap();
        assert!(listed.iter().any(|e| e.path.trim_end_matches('/') == "proje/sub/deep/c.txt" && !e.is_dir), "{listed:?}");
        let big = listed.iter().find(|e| e.path.ends_with("big.bin")).unwrap();
        assert_eq!(big.size, 700_000);

        let dest = smart_extract_dir(&arch);
        assert_eq!(dest.file_name().unwrap().to_string_lossy(), name.split('.').next().unwrap());
        let mut prog = Vec::new();
        let es = extract(&arch, &dest, &mut |p| prog.push(p), &|| false).unwrap();
        assert_eq!(es.files, 4);
        assert_eq!(es.renamed, 0);
        assert!(!prog.is_empty());
        assert_same(&root, &dest.join("proje"));
        assert!(dest.join("proje/empty").is_dir());

        // mtime survives (seconds precision is enough; zip is 2 s)
        let m1 = fs::metadata(root.join("a.txt")).unwrap().modified().unwrap();
        let m2 = fs::metadata(dest.join("proje/a.txt")).unwrap().modified().unwrap();
        let diff = m1.duration_since(m2).or_else(|e| Ok::<_, ()>(e.duration())).unwrap();
        assert!(diff.as_secs() <= 2, "mtime kaydı: {diff:?}");

        // Second extraction into the same dir auto-renames instead of overwriting.
        let es2 = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es2.renamed, 4);
        assert!(dest.join("proje/a (2).txt").exists());
        assert_eq!(fs::read(dest.join("proje/a (2).txt")).unwrap(), b"merhaba dunya");
        assert_eq!(smart_extract_dir(&arch).file_name().unwrap().to_string_lossy(), format!("{} (2)", name.split('.').next().unwrap()));

        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn zip_roundtrip() {
        roundtrip(Format::Zip, "proje.zip");
    }

    #[test]
    fn sevenz_roundtrip() {
        roundtrip(Format::SevenZ, "proje.7z");
    }

    #[test]
    fn tar_zst_roundtrip() {
        roundtrip(Format::TarZst, "proje.tar.zst");
    }

    #[test]
    fn zip_slip_rejected() {
        let out = tmp("slip");
        let arch = out.join("kotu.zip");
        {
            let mut zw = zip::ZipWriter::new(File::create(&arch).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            zw.start_file("iyi.txt", o).unwrap();
            zw.write_all(b"ok").unwrap();
            zw.start_file("../evil.txt", o).unwrap();
            zw.write_all(b"pwn").unwrap();
            zw.finish().unwrap();
        }
        let dest = out.join("hedef");
        let err = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap_err();
        assert!(err.to_string().contains("Güvensiz"), "{err}");
        assert!(!out.join("evil.txt").exists());
        assert!(!dest.join("iyi.txt").exists(), "doğrulama yazmadan önce yapılmalı");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn sanitize_cases() {
        assert!(sanitize("../x").is_err());
        assert!(sanitize("a/../../x").is_err());
        assert!(sanitize("/etc/passwd").is_err());
        assert!(sanitize("C:/Windows/x").is_err());
        assert!(sanitize("a\\..\\b").is_err());
        assert!(sanitize(".. /x").is_err());
        assert!(sanitize("file.txt:stream").is_err());
        assert_eq!(sanitize("./a//b/").unwrap().unwrap(), PathBuf::from("a").join("b"));
        assert_eq!(sanitize("./").unwrap(), None);
    }

    #[test]
    fn cancel_cleans_up() {
        let root = make_tree();
        let out = tmp("cancel");
        let arch = out.join("x.zip");
        let err = compress(&[root.clone()], &arch, Format::Zip, 5, &mut |_| {}, &|| true).unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some());
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn gz_and_tar_gz_read() {
        let out = tmp("gz");
        // single .gz
        let gz = out.join("not.txt.gz");
        {
            let mut e = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
            e.write_all(b"gz icerik").unwrap();
            e.finish().unwrap();
        }
        let l = list(&gz).unwrap();
        assert_eq!((l[0].path.as_str(), l[0].size), ("not.txt", 9));
        let dest = out.join("d");
        extract(&gz, &dest, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(dest.join("not.txt")).unwrap(), b"gz icerik");

        // tar.gz
        let tgz = out.join("p.tar.gz");
        {
            let enc = flate2::write::GzEncoder::new(File::create(&tgz).unwrap(), flate2::Compression::default());
            let mut tb = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o644);
            tb.append_data(&mut h, "k/x.txt", &b"abc"[..]).unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(list(&tgz).unwrap()[0].path, "k/x.txt");
        let dest = smart_extract_dir(&tgz);
        assert_eq!(dest.file_name().unwrap(), "p");
        extract(&tgz, &dest, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(dest.join("k/x.txt")).unwrap(), b"abc");
        let _ = fs::remove_dir_all(&out);
    }

    fn pw_opts(p: &str) -> Options {
        Options { password: Some(p.to_string()), ..Default::default() }
    }

    fn is_pw(e: &anyhow::Error, want: PasswordError) -> bool {
        e.downcast_ref::<PasswordError>() == Some(&want)
    }

    fn password_roundtrip(format: Format, name: &str) {
        let root = make_tree();
        let out = tmp("pw");
        let arch = out.join(name);
        let co = CompressOptions { password: Some("gizli-şifre 1".into()) };
        compress_with(&[root.clone()], &arch, format, 5, &co, &mut |_| {}, &|| false).unwrap();
        assert!(needs_password(&arch).unwrap());

        // no password / wrong password: typed errors, nothing written
        let dest = out.join("d");
        let e = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap_err();
        assert!(is_pw(&e, PasswordError::Required), "{e:?}");
        let e = extract_with(&arch, &dest, &pw_opts("yanlis"), &mut |_| {}, &|| false).unwrap_err();
        assert!(is_pw(&e, PasswordError::Wrong), "{e:?}");
        assert!(!dest.join("proje/a.txt").exists());

        // listing: zip names are public; 7z hides them behind the header encryption
        match format {
            Format::Zip => {
                let l = list(&arch).unwrap();
                assert!(l.iter().any(|e| e.path.ends_with("a.txt")));
                // members are marked encrypted (the list shows a lock); folders are not
                assert!(l.iter().filter(|e| !e.is_dir).all(|e| e.encrypted), "{l:?}");
            }
            _ => {
                let e = list(&arch).unwrap_err();
                assert!(is_pw(&e, PasswordError::Required), "{e:?}");
                let e = list_with(&arch, &pw_opts("yanlis")).unwrap_err();
                assert!(is_pw(&e, PasswordError::Wrong), "{e:?}");
            }
        }
        let l = list_with(&arch, &pw_opts("gizli-şifre 1")).unwrap();
        assert!(l.iter().any(|e| e.path.trim_end_matches('/') == "proje/sub/deep/c.txt"), "{l:?}");

        let es = extract_with(&arch, &dest, &pw_opts("gizli-şifre 1"), &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.files, 4);
        assert_same(&root, &dest.join("proje"));
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn zip_password_roundtrip() {
        password_roundtrip(Format::Zip, "p.zip");
    }

    #[test]
    fn sevenz_password_roundtrip() {
        password_roundtrip(Format::SevenZ, "p.7z");
    }

    #[test]
    fn zip_zipcrypto_read() {
        // Legacy ZipCrypto archive (what older tools write).
        let out = tmp("zc");
        let arch = out.join("z.zip");
        {
            let mut zw = zip::ZipWriter::new(File::create(&arch).unwrap());
            use zip::unstable::write::FileOptionsExt;
            let o = zip::write::SimpleFileOptions::default().with_deprecated_encryption(b"pw").unwrap();
            zw.start_file("x.txt", o).unwrap();
            zw.write_all(b"zipcrypto icerik").unwrap();
            zw.finish().unwrap();
        }
        assert!(needs_password(&arch).unwrap());
        let e = extract(&arch, &out.join("d"), &mut |_| {}, &|| false).unwrap_err();
        assert!(is_pw(&e, PasswordError::Required));
        let e = extract_with(&arch, &out.join("d"), &pw_opts("nope"), &mut |_| {}, &|| false).unwrap_err();
        assert!(is_pw(&e, PasswordError::Wrong), "{e:?}");
        extract_with(&arch, &out.join("d"), &pw_opts("pw"), &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(out.join("d/x.txt")).unwrap(), b"zipcrypto icerik");
        let _ = fs::remove_dir_all(&out);
    }

    /// The password is checked up front (ZIP lists fine without one, so a listing can't tell).
    #[test]
    fn check_password_zip_and_7z() {
        let root = make_tree();
        for (fmt, name) in [(Format::Zip, "c.zip"), (Format::SevenZ, "c.7z")] {
            let out = tmp("chk");
            let arch = out.join(name);
            let co = CompressOptions { password: Some("doğru".into()) };
            compress_with(&[root.clone()], &arch, fmt, 5, &co, &mut |_| {}, &|| false).unwrap();
            check_password(&arch, &pw_opts("doğru")).unwrap();
            let e = check_password(&arch, &pw_opts("yanlış")).unwrap_err();
            assert!(is_pw(&e, PasswordError::Wrong), "{name}: {e:?}");
            let _ = fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn password_unsupported_for_tar_zst_and_plain_probe() {
        let root = make_tree();
        let out = tmp("pwz");
        let co = CompressOptions { password: Some("x".into()) };
        let e = compress_with(&[root.clone()], &out.join("a.tar.zst"), Format::TarZst, 5, &co, &mut |_| {}, &|| false).unwrap_err();
        assert!(e.to_string().contains("parolayı desteklemiyor"));
        assert!(!out.join("a.tar.zst").exists());
        compress(&[root.clone()], &out.join("a.7z"), Format::SevenZ, 3, &mut |_| {}, &|| false).unwrap();
        compress(&[root.clone()], &out.join("a.zip"), Format::Zip, 3, &mut |_| {}, &|| false).unwrap();
        assert!(!needs_password(&out.join("a.7z")).unwrap() && !needs_password(&out.join("a.zip")).unwrap());
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    /// Splits `file` into `n` parts `<stem>.001`, `.002`, ... in `dir`; returns the first part.
    fn split_into(file: &Path, dir: &Path, stem: &str, n: usize) -> PathBuf {
        let data = fs::read(file).unwrap();
        let chunk = data.len().div_ceil(n);
        for (i, c) in data.chunks(chunk).enumerate() {
            fs::write(dir.join(format!("{stem}.{:03}", i + 1)), c).unwrap();
        }
        dir.join(format!("{stem}.001"))
    }

    fn split_roundtrip(format: Format, name: &str, split_stem: &str, password: Option<&str>) {
        let root = make_tree();
        let out = tmp("split");
        let arch = out.join(name);
        let co = CompressOptions { password: password.map(str::to_string) };
        compress_with(&[root.clone()], &arch, format, 0, &co, &mut |_| {}, &|| false).unwrap();
        let parts = out.join("parts");
        fs::create_dir_all(&parts).unwrap();
        let first = split_into(&arch, &parts, split_stem, 3);
        assert_eq!(volume_set(&first).len(), 3);
        assert_eq!(volume_set(&parts.join(format!("{split_stem}.002"))).len(), 3);
        assert!(is_supported(&first), "{first:?}");
        assert!(!is_supported(&parts.join(format!("{split_stem}.002"))));
        let opts = Options { password: password.map(str::to_string), ..Default::default() };
        let l = list_with(&first, &opts).unwrap();
        assert!(l.iter().any(|e| e.path.trim_end_matches('/') == "proje/sub/b.txt"), "{l:?}");
        let dest = out.join("d");
        let es = extract_with(&first, &dest, &opts, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.files, 4);
        assert_same(&root, &dest.join("proje"));
        assert_eq!(needs_password(&first).unwrap(), password.is_some());
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn split_7z_volumes() {
        split_roundtrip(Format::SevenZ, "a.7z", "a.7z", None);
    }

    #[test]
    fn split_zip_volumes() {
        split_roundtrip(Format::Zip, "a.zip", "a.zip", None);
    }

    #[test]
    fn split_name_without_inner_extension_uses_magic() {
        split_roundtrip(Format::SevenZ, "a.7z", "yedek", None);
        split_roundtrip(Format::Zip, "a.zip", "yedek", None);
    }

    #[test]
    fn split_encrypted_volumes() {
        split_roundtrip(Format::SevenZ, "a.7z", "s.7z", Some("pw 1"));
        split_roundtrip(Format::Zip, "a.zip", "s.zip", Some("pw 1"));
    }

    #[test]
    fn split_tar_zst_volumes_and_extract_dir_name() {
        let root = make_tree();
        let out = tmp("splittar");
        let arch = out.join("t.tar.zst");
        compress(&[root.clone()], &arch, Format::TarZst, 3, &mut |_| {}, &|| false).unwrap();
        let first = split_into(&arch, &out, "t.tar.zst", 2);
        assert_eq!(smart_extract_dir(&first).file_name().unwrap().to_string_lossy(), "t");
        let dest = out.join("d");
        assert_eq!(extract(&first, &dest, &mut |_| {}, &|| false).unwrap().files, 4);
        assert_same(&root, &dest.join("proje"));
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn volume_reader_seek_and_read() {
        let out = tmp("vr");
        fs::write(out.join("x.001"), b"0123").unwrap();
        fs::write(out.join("x.002"), b"").unwrap();
        fs::write(out.join("x.003"), b"456789").unwrap();
        let set = volume_set(&out.join("x.001"));
        assert_eq!(set.len(), 3);
        let mut r = volume::VolumeReader::open(&set).unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"0123456789");
        r.seek(SeekFrom::Start(3)).unwrap();
        let mut b = [0u8; 3];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"345");
        assert_eq!(r.seek(SeekFrom::End(-2)).unwrap(), 8);
        let mut t = Vec::new();
        r.read_to_end(&mut t).unwrap();
        assert_eq!(t, b"89");
        let _ = fs::remove_dir_all(&out);
    }

    // RAR fixtures are generated by the tiny RAR 5 "store" writer in rar::fixture (no UnRAR
    // binary / network needed). Encrypted RAR cannot be generated here, so password handling
    // for RAR is covered only by the error mapping, not by a round trip.
    #[test]
    fn rar_list_extract_single_volume() {
        use rar::fixture::{Item, volume};
        let out = tmp("rar");
        let arch = out.join("proje.rar");
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(
            &arch,
            volume(
                &[
                    Item::Dir("proje"),
                    Item::File("proje/a.txt", b"merhaba rar"),
                    Item::Dir("proje/sub"),
                    Item::File("proje/sub/b\u{15f}.bin", &big),
                ],
                0,
                false,
            ),
        )
        .unwrap();
        assert!(is_supported(&arch));
        assert!(!needs_password(&arch).unwrap());
        let l = list(&arch).unwrap();
        assert_eq!(l.len(), 4, "{l:?}");
        assert!(l.iter().any(|e| e.path == "proje" && e.is_dir));
        let b = l.iter().find(|e| e.path == "proje/sub/b\u{15f}.bin").unwrap();
        assert_eq!((b.size, b.is_dir), (300_000, false));

        let dest = smart_extract_dir(&arch);
        assert_eq!(dest.file_name().unwrap(), "proje");
        let es = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.files, 2);
        assert_eq!(fs::read(dest.join("proje/a.txt")).unwrap(), b"merhaba rar");
        assert_eq!(fs::read(dest.join("proje/sub/b\u{15f}.bin")).unwrap(), big);
        // never overwrite
        let es = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.renamed, 2);
        assert!(dest.join("proje/a (2).txt").exists());
        // cancellation leaves no temp / partial files
        let c = extract(&arch, &out.join("c"), &mut |_| {}, &|| true).unwrap_err();
        assert!(c.downcast_ref::<Cancelled>().is_some(), "{c:?}");
        let _ = fs::remove_dir_all(&out);
    }

    /// Real WinRAR archives (CC0 corpus in rarchive's tests) through our own decoder: contents,
    /// times, and the solid / compressed paths.
    #[test]
    fn rar_winrar_archives_via_rarchive() {
        // third-party (CC0) archives: not in the repository, see rarchive's tests/data/winrar
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/winrar");
        if !corpus.join("testfile.rar5.rar").exists() {
            eprintln!("skipped: tests/data/winrar is not in the repository");
            return;
        }
        // RAR 5 and RAR 3.x (WinRAR's own), plain, solid, with recovery record, locked
        for name in [
            "testfile.rar5.rar",
            "testfile.rar5.solid.cbr",
            "testfile.rar5.rr.cbr",
            "testfile.rar3.solid.rar",
            "testfile.rar3.rr.rar",
            "testfile.rar3.locked.rar",
            "testfile.rar3.av.rar",
        ] {
            let out = tmp("rarw");
            let es = extract(&corpus.join(name), &out, &mut |_| {}, &|| false).unwrap();
            assert!(es.files >= 1, "{name}");
            for e in list(&corpus.join(name)).unwrap() {
                let got = fs::read(out.join(&e.path)).unwrap();
                assert_eq!(got, fs::read(corpus.join(&e.path)).unwrap(), "{name}: {}", e.path);
                assert!(e.modified_ms > 946_684_800_000, "{name}: {} has no time", e.path);
                // the stored time lands on the file
                let mt = fs::metadata(out.join(&e.path)).unwrap().modified().unwrap();
                assert_eq!(mtime_ms(mt) / 1000, e.modified_ms / 1000, "{name}: {}", e.path);
            }
            let _ = fs::remove_dir_all(&out);
        }
    }

    /// Encrypted RAR 4 through rarchive: there is no check value, yet a wrong password is still
    /// refused up front (a small member is decoded to tell) and nothing gets written.
    #[test]
    fn rar4_encrypted_via_rarchive() {
        // made by the rars crate, checked with UnRAR (rarchive's tests/data/rar4/enc.rar)
        let arch = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/rar4-enc.rar");
        let pw = |p: &str| Options { password: Some(p.into()), ..Default::default() };
        assert!(needs_password(&arch).unwrap());
        let e = check_password(&arch, &pw("yanlış")).unwrap_err();
        assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Wrong), "{e:?}");
        let out = tmp("rar4enc");
        let e = extract_with(&arch, &out, &pw("yanlış"), &mut |_| {}, &|| false).unwrap_err();
        assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Wrong), "{e:?}");
        assert!(fs::read_dir(&out).map_or(true, |mut d| d.next().is_none()), "nothing written");
        check_password(&arch, &pw("parola ✓ 123")).unwrap();
        let es = extract_with(&arch, &out, &pw("parola ✓ 123"), &mut |_| {}, &|| false).unwrap();
        assert!(es.files >= 1);
        let _ = fs::remove_dir_all(&out);
    }

    /// Encrypted RAR 5 through rarchive: password asked, checked up front, contents right.
    #[test]
    fn rar_encrypted_via_rarchive() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/generated");
        let pw = |p: &str| Options { password: Some(p.into()), ..Default::default() };
        for name in ["enc-files.rar", "enc-solid-headers.rar"] {
            let arch = corpus.join(name);
            assert!(needs_password(&arch).unwrap(), "{name}");
            let out = tmp("rarenc");
            let e = extract_with(&arch, &out, &Options::default(), &mut |_| {}, &|| false).unwrap_err();
            assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Required), "{name}: {e:?}");
            let e = check_password(&arch, &pw("yanlış")).unwrap_err();
            assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Wrong), "{name}: {e:?}");
            let e = extract_with(&arch, &out, &pw("yanlış"), &mut |_| {}, &|| false).unwrap_err();
            assert_eq!(e.downcast_ref::<PasswordError>(), Some(&PasswordError::Wrong), "{name}: {e:?}");
            assert!(fs::read_dir(&out).map_or(true, |mut d| d.next().is_none()), "{name}: wrote files with a wrong password");
            check_password(&arch, &pw("şifre 123")).unwrap();
            let es = extract_with(&arch, &out, &pw("şifre 123"), &mut |_| {}, &|| false).unwrap();
            assert_eq!(es.files, 3, "{name}");
            for f in ["a.txt", "klasör/b.bin", "klasör/boş.txt"] {
                assert_eq!(fs::read(out.join(f)).unwrap(), fs::read(corpus.join("src").join(f)).unwrap(), "{name}: {f}");
            }
            let _ = fs::remove_dir_all(&out);
        }
    }

    /// Members split across volumes (and an encrypted set) through rarchive, from a middle volume.
    #[test]
    fn rar_split_volumes_via_rarchive() {
        let vols = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/volumes");
        for (open, pw) in [("vol.part2.rar", None), ("vol-solid.r00", None), ("vol-enc.part3.rar", Some("parça şifresi"))] {
            let out = tmp("rarvol");
            let opts = Options { password: pw.map(Into::into), ..Default::default() };
            let es = extract_with(&vols.join(open), &out, &opts, &mut |_| {}, &|| false).unwrap();
            assert_eq!(es.files, 3, "{open}");
            for f in ["gürültü.bin", "metin.txt", "küçük.txt"] {
                assert_eq!(fs::read(out.join(f)).unwrap(), fs::read(vols.join("src").join(f)).unwrap(), "{open}: {f}");
            }
            let _ = fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn rar_multi_volume_part_naming() {
        use rar::fixture::{Item, volume};
        let out = tmp("rarmv");
        fs::write(out.join("m.part1.rar"), volume(&[Item::File("bir.txt", b"1")], 0, true)).unwrap();
        fs::write(out.join("m.part2.rar"), volume(&[Item::File("iki.txt", b"22")], 1, false)).unwrap();
        for open in ["m.part1.rar", "m.part2.rar"] {
            let p = out.join(open);
            assert!(is_supported(&p));
            let names: Vec<String> = list(&p).unwrap().into_iter().map(|e| e.path).collect();
            assert_eq!(names, ["bir.txt", "iki.txt"], "{open}");
        }
        assert_eq!(volume_set(&out.join("m.part2.rar")).len(), 2);
        assert_eq!(smart_extract_dir(&out.join("m.part1.rar")).file_name().unwrap(), "m");
        let dest = out.join("d");
        assert_eq!(extract(&out.join("m.part2.rar"), &dest, &mut |_| {}, &|| false).unwrap().files, 2);
        assert_eq!(fs::read(dest.join("iki.txt")).unwrap(), b"22");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rar_old_style_volume_names() {
        use rar::fixture::{Item, volume};
        let out = tmp("rarold");
        fs::write(out.join("o.rar"), volume(&[Item::File("a.txt", b"a")], 0, true)).unwrap();
        fs::write(out.join("o.r00"), volume(&[Item::File("b.txt", b"b")], 1, false)).unwrap();
        assert_eq!(volume_set(&out.join("o.r00")).len(), 2);
        assert_eq!(volume_set(&out.join("o.rar")).len(), 2);
        assert!(is_supported(&out.join("o.r00")));
        let names: Vec<String> = list(&out.join("o.r00")).unwrap().into_iter().map(|e| e.path).collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rar_garbage_is_an_error_not_a_panic() {
        let out = tmp("rarbad");
        fs::write(out.join("kotu.rar"), b"bu bir rar degil").unwrap();
        assert!(list(&out.join("kotu.rar")).is_err());
        let e = extract(&out.join("kotu.rar"), &out.join("d"), &mut |_| {}, &|| false).unwrap_err();
        assert!(e.to_string().contains("arşiv değil"), "{e}");
        let _ = fs::remove_dir_all(&out);
    }

    /// What can't be opened says why, in the user's words, and writes nothing.
    #[test]
    fn rar_refusals_explain_themselves() {
        let out = tmp("rarwhy");
        // a set with its last part missing
        let vols = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/volumes");
        for f in ["vol.part1.rar", "vol.part2.rar"] {
            fs::copy(vols.join(f), out.join(f)).unwrap();
        }
        let dest = out.join("d");
        let e = extract(&out.join("vol.part1.rar"), &dest, &mut |_| {}, &|| false).unwrap_err();
        assert!(e.to_string().contains("parçası eksik"), "{e}");
        assert!(!dest.exists() || fs::read_dir(&dest).unwrap().next().is_none(), "nothing written");
        // the names still list
        assert!(!list(&out.join("vol.part1.rar")).unwrap().is_empty());

        // RAR 1.3 / 1.4 (signature "RE~^")
        let mut old = b"RE~^".to_vec();
        old.extend_from_slice(&[0u8; 64]);
        fs::write(out.join("eski.rar"), &old).unwrap();
        let e = extract(&out.join("eski.rar"), &out.join("d2"), &mut |_| {}, &|| false).unwrap_err();
        assert!(e.to_string().contains("RAR 1.3/1.4"), "{e}");
        let _ = fs::remove_dir_all(&out);
    }

    /// Builds a tiny ISO with hadris-iso's writer (dev-dependency only).
    fn make_iso(path: &Path, joliet: bool, rock_ridge: bool) -> Vec<u8> {
        use hadris_fs::{Content, Node, Tree};
        use hadris_iso::{IsoId, IsoOptions};
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        let mut tree = Tree::new();
        tree.insert("readme.txt", Node::file(Content::bytes("merhaba iso"))).unwrap();
        tree.insert("docs/big.bin", Node::file(Content::bytes(big.clone()))).unwrap();
        tree.insert("docs/T\u{fc}rk\u{e7}e uzun dosya ad\u{131}.txt", Node::file(Content::bytes("uzun ad"))).unwrap();
        tree.insert("bos", Node::dir()).unwrap();
        tree.insert("a/b/c/derin.txt", Node::file(Content::bytes("derin"))).unwrap();
        let mut o = IsoOptions::default().with_id(IsoId::Volume, "TEST");
        if joliet {
            o = o.with_joliet();
        }
        if rock_ridge {
            o = o.with_rock_ridge();
        }
        let file = fs::File::options().read(true).write(true).create(true).truncate(true).open(path).unwrap();
        hadris_iso::sync::write(hadris_storage::host::FileDevice::new(file).unwrap(), &tree, &o).unwrap();
        big
    }

    fn check_iso(first: &Path, big: &[u8], long_names: bool) {
        let l = list(first).unwrap();
        let find = |n: &str| l.iter().find(|e| e.path.eq_ignore_ascii_case(n));
        assert!(find("docs").is_some_and(|e| e.is_dir), "{l:?}");
        assert!(find("bos").is_some_and(|e| e.is_dir), "{l:?}");
        assert_eq!(find("docs/big.bin").map(|e| (e.size, e.is_dir)), Some((100_000, false)), "{l:?}");
        assert!(find("a/b/c/derin.txt").is_some(), "{l:?}");
        let long = "docs/T\u{fc}rk\u{e7}e uzun dosya ad\u{131}.txt";
        assert_eq!(find(long).is_some(), long_names, "{l:?}");
        assert!(l.iter().all(|e| !e.path.contains(';')), "{l:?}");

        let out = first.parent().unwrap().join("cikti");
        let es = extract(first, &out, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.files, if long_names { 4 } else { 3 + 1 });
        let docs = fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
        assert!(docs.contains(&"docs".to_string()), "{docs:?}");
        let docs_dir = fs::read_dir(&out).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("docs")).unwrap();
        let bigf = fs::read_dir(&docs_dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("big.bin")).unwrap();
        assert_eq!(fs::read(bigf).unwrap(), big);
        // never overwrite
        let es2 = extract(first, &out, &mut |_| {}, &|| false).unwrap();
        assert_eq!(es2.renamed, es.files);
    }

    #[test]
    fn iso_joliet_and_rock_ridge_and_primary() {
        for (joliet, rr, long) in [(true, false, true), (true, true, true), (false, false, false)] {
            let out = tmp("iso");
            let iso = out.join("disk.iso");
            let big = make_iso(&iso, joliet, rr);
            assert!(is_supported(&iso));
            check_iso(&iso, &big, long);
            if joliet && !rr {
                // exact long Unicode name survives
                let l = list(&iso).unwrap();
                assert!(l.iter().any(|e| e.path == "docs/T\u{fc}rk\u{e7}e uzun dosya ad\u{131}.txt"), "{l:?}");
            }
            let _ = fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn iso_split_volumes() {
        let out = tmp("isosplit");
        let iso = out.join("disk.iso");
        let big = make_iso(&iso, true, false);
        let parts = out.join("p");
        fs::create_dir_all(&parts).unwrap();
        let first = split_into(&iso, &parts, "disk.iso", 3);
        check_iso(&first, &big, true);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn udf_only_image_in_iso_file() {
        use hadris_fs::{Content, Node, Tree};
        use hadris_udf::{UdfId, UdfOptions};
        let out = tmp("udf");
        let mut tree = Tree::new();
        tree.insert("Windows Kurulum/sources/install.wim", Node::file(Content::bytes(vec![9u8; 50_000]))).unwrap();
        tree.insert("T\u{fc}rk\u{e7}e.txt", Node::file(Content::bytes("udf"))).unwrap();
        let o = UdfOptions::default().with_id(UdfId::Volume, "UDFTEST");
        let file = fs::File::options().read(true).write(true).create(true).truncate(true).open(out.join("w.iso")).unwrap();
        hadris_udf::sync::write(hadris_storage::host::FileDevice::new(file).unwrap(), &tree, &o).unwrap();
        let l = list(&out.join("w.iso")).unwrap();
        assert!(l.iter().any(|e| e.path == "Windows Kurulum/sources/install.wim" && e.size == 50_000), "{l:?}");
        assert!(l.iter().any(|e| e.path == "T\u{fc}rk\u{e7}e.txt"), "{l:?}");
        let es = extract(&out.join("w.iso"), &out.join("d"), &mut |_| {}, &|| false).unwrap();
        assert_eq!(es.files, 2);
        assert_eq!(fs::read(out.join("d/Windows Kurulum/sources/install.wim")).unwrap(), vec![9u8; 50_000]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn iso_garbage_is_an_error() {
        let out = tmp("isobad");
        fs::write(out.join("x.iso"), vec![7u8; 100_000]).unwrap();
        assert!(list(&out.join("x.iso")).is_err());
        let _ = fs::remove_dir_all(&out);
    }

    // ------------------------------------------------------------ editing

    fn make_tar_variant(path: &Path, kind: &str, root: &Path) {
        let items = collect(&[root.to_path_buf()], &[]).unwrap();
        let mut sink = |_: Progress| {};
        let mut ctx = CompressCtx {
            progress: &mut sink,
            cancelled: &|| false,
            done: 0,
            total: 0,
            last_emit: 0,
            was_cancelled: false,
            current: String::new(),
        };
        let f = BufWriter::new(File::create(path).unwrap());
        macro_rules! fill {
            ($w:expr) => {{
                let mut tb = tar::Builder::new($w);
                for i in &items {
                    tar_put(&mut tb, i, &mut ctx).unwrap();
                }
                tb.into_inner().unwrap()
            }};
        }
        match kind {
            "tar" => fill!(f).flush().unwrap(),
            "tar.gz" => {
                fill!(flate2::write::GzEncoder::new(f, flate2::Compression::new(3))).finish().unwrap().flush().unwrap()
            }
            "tar.xz" => {
                fill!(lzma_rust2::XzWriter::new(f, lzma_rust2::XzOptions::with_preset(1)).unwrap())
                    .finish()
                    .unwrap()
                    .flush()
                    .unwrap()
            }
            _ => unreachable!(),
        }
    }

    fn paths_of(arch: &Path, opts: &Options) -> Vec<String> {
        let mut v: Vec<String> =
            list_with(arch, opts).unwrap().into_iter().map(|e| e.path.trim_end_matches('/').to_string()).collect();
        v.sort();
        v
    }

    /// Creates an archive from the sample tree (plus an empty file), then exercises add / delete.
    fn edit_roundtrip(name: &str, make: &dyn Fn(&Path, &Path), opts: &Options) {
        let root = make_tree();
        fs::write(root.join("sifir.txt"), b"").unwrap();
        let out = tmp("edit");
        let arch = out.join(name);
        make(&arch, &root);
        assert!(is_editable(&arch), "{name}");
        let before = paths_of(&arch, opts);

        // sources: folder "ek" (x.txt, a.txt), file "b.txt" (collides with proje/sub/b.txt)
        let src = tmp("editsrc");
        fs::create_dir_all(src.join("ek")).unwrap();
        fs::write(src.join("ek/x.txt"), "yeni x").unwrap();
        fs::write(src.join("ek/a.txt"), "yeni a").unwrap();
        fs::write(src.join("b.txt"), "yeni b").unwrap();

        // 1. add into a sub folder; collision gets "(2)"
        let st = add(&arch, &[src.join("ek"), src.join("b.txt")], "proje/sub", opts, &mut |_| {}, &|| false).unwrap();
        assert_eq!((st.added_files, st.added_dirs, st.renamed), (3, 1, 1), "{st:?}");
        let after = paths_of(&arch, opts);
        for want in ["proje/sub/ek", "proje/sub/ek/x.txt", "proje/sub/ek/a.txt", "proje/sub/b (2).txt", "proje/sub/b.txt", "proje/a.txt", "proje/sifir.txt"] {
            assert!(after.iter().any(|p| p == want), "{name}: {want} yok: {after:?}");
        }
        assert_eq!(after.len(), before.len() + 4, "{name}");
        assert!(!out.join(format!("{name}.part")).exists());

        // 2. adding the same folder again renames the folder, not the files
        let st = add(&arch, &[src.join("ek")], "proje/sub", opts, &mut |_| {}, &|| false).unwrap();
        assert_eq!(st.renamed, 1);
        assert!(paths_of(&arch, opts).iter().any(|p| p == "proje/sub/ek (2)/x.txt"));

        // 3. add at the root
        add(&arch, &[src.join("b.txt")], "", opts, &mut |_| {}, &|| false).unwrap();
        assert!(paths_of(&arch, opts).iter().any(|p| p == "b.txt"));

        // 4. content survives the rewrite
        let dest = out.join("d1");
        extract_with(&arch, &dest, opts, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(dest.join("proje/sub/ek/x.txt")).unwrap(), b"yeni x");
        assert_eq!(fs::read(dest.join("proje/sub/b (2).txt")).unwrap(), b"yeni b");
        assert_eq!(fs::read(dest.join("proje/sub/b.txt")).unwrap(), b"ikinci");
        assert_eq!(fs::read(dest.join("proje/big.bin")).unwrap(), fs::read(root.join("big.bin")).unwrap());
        assert_eq!(fs::read(dest.join("proje/sifir.txt")).unwrap(), b"");
        assert_eq!(fs::read(dest.join("proje/sub/deep/c.txt")).unwrap(), "çğüşiö unicode içerik".as_bytes());
        assert!(dest.join("proje/empty").is_dir());

        // 5. delete a folder (everything below) and a file
        let bytes_before = fs::read(&arch).unwrap();
        let st = delete(&arch, &["proje/sub".into(), "proje/a.txt".into()], opts, &mut |_| {}, &|| false).unwrap();
        assert!(st.removed >= 6, "{name}: {st:?}");
        let left = paths_of(&arch, opts);
        assert!(!left.iter().any(|p| p.starts_with("proje/sub") || p == "proje/a.txt"), "{name}: {left:?}");
        assert!(left.iter().any(|p| p == "proje/big.bin") && left.iter().any(|p| p == "b.txt"), "{name}: {left:?}");
        let dest = out.join("d2");
        extract_with(&arch, &dest, opts, &mut |_| {}, &|| false).unwrap();
        assert!(!dest.join("proje/sub").exists() && !dest.join("proje/a.txt").exists());
        assert_eq!(fs::read(dest.join("proje/big.bin")).unwrap().len(), 700_000);
        assert_ne!(fs::read(&arch).unwrap(), bytes_before);

        // 6. nothing matches: archive untouched, no .part
        let same = fs::read(&arch).unwrap();
        let st = delete(&arch, &["yok/boyle/bir/yol".into()], opts, &mut |_| {}, &|| false).unwrap();
        assert_eq!(st.removed, 0);
        assert_eq!(fs::read(&arch).unwrap(), same);

        // 7. cancellation keeps the original and leaves no .part
        let e = add(&arch, &[src.join("ek")], "", opts, &mut |_| {}, &|| true).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{name}: {e:?}");
        let e = delete(&arch, &["proje".into()], opts, &mut |_| {}, &|| true).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{name}: {e:?}");
        assert_eq!(fs::read(&arch).unwrap(), same);
        assert!(!out.join(format!("{name}.part")).exists());
        assert_eq!(fs::read_dir(&out).unwrap().filter(|e| e.as_ref().unwrap().path().is_file()).count(), 1);

        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&out);
    }

    fn make_with(format: Format) -> impl Fn(&Path, &Path) {
        move |arch, root| {
            compress(&[root.to_path_buf()], arch, format, 3, &mut |_| {}, &|| false).unwrap();
        }
    }

    #[test]
    fn edit_zip() {
        edit_roundtrip("e.zip", &make_with(Format::Zip), &Options::default());
    }

    #[test]
    fn edit_7z() {
        edit_roundtrip("e.7z", &make_with(Format::SevenZ), &Options::default());
    }

    #[test]
    fn edit_tar_zst() {
        edit_roundtrip("e.tar.zst", &make_with(Format::TarZst), &Options::default());
    }

    /// tar.bz2 (we don't create it): made here with the tar + bzip2 crates, then the same
    /// list / extract / add / delete round trip as the others.
    #[test]
    fn edit_tar_bz2() {
        let make = |arch: &Path, root: &Path| {
            let f = fs::File::create(arch).unwrap();
            let enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::new(6));
            let mut b = tar::Builder::new(enc);
            let name = root.file_name().unwrap().to_string_lossy().into_owned();
            b.append_dir_all(&name, root).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        };
        edit_roundtrip("e.tar.bz2", &make, &Options::default());
    }

    #[test]
    fn single_bz2_file() {
        let d = tmp("bz2one");
        let arch = d.join("not defteri.txt.bz2");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
        let mut enc = bzip2::write::BzEncoder::new(fs::File::create(&arch).unwrap(), bzip2::Compression::new(9));
        std::io::Write::write_all(&mut enc, &data).unwrap();
        enc.finish().unwrap();
        assert!(is_supported(&arch));
        let out = d.join("çıktı");
        extract(&arch, &out, &mut |_| {}, &|| false).unwrap();
        assert_eq!(fs::read(out.join("not defteri.txt")).unwrap(), data);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_tar_family() {
        for (name, kind) in [("e.tar", "tar"), ("e.tar.gz", "tar.gz"), ("e.tar.xz", "tar.xz")] {
            edit_roundtrip(name, &|arch, root| make_tar_variant(arch, kind, root), &Options::default());
        }
    }

    #[test]
    fn edit_encrypted_archives_keep_protection() {
        let pw = Options { password: Some("edit-pw".into()), ..Default::default() };
        for (name, format) in [("p.zip", Format::Zip), ("p.7z", Format::SevenZ)] {
            let co = CompressOptions { password: Some("edit-pw".into()) };
            edit_roundtrip(name, &|arch, root| {
                compress_with(&[root.to_path_buf()], arch, format, 3, &co, &mut |_| {}, &|| false).unwrap();
            }, &pw);

            // protection state survives an edit; wrong password leaves the archive alone
            let root = make_tree();
            let out = tmp("editpw");
            let arch = out.join(name);
            compress_with(&[root.clone()], &arch, format, 3, &co, &mut |_| {}, &|| false).unwrap();
            let src = out.join("n.txt");
            fs::write(&src, "gizli ek").unwrap();
            let orig = fs::read(&arch).unwrap();
            let bad = Options { password: Some("yanlis".into()), ..Default::default() };
            if format == Format::SevenZ {
                let e = add(&arch, &[src.clone()], "", &bad, &mut |_| {}, &|| false).unwrap_err();
                assert!(is_pw(&e, PasswordError::Wrong), "{e:?}");
                let e = add(&arch, &[src.clone()], "", &Options::default(), &mut |_| {}, &|| false).unwrap_err();
                assert!(is_pw(&e, PasswordError::Required), "{e:?}");
                assert_eq!(fs::read(&arch).unwrap(), orig);
            }
            add(&arch, &[src.clone()], "proje", &pw, &mut |_| {}, &|| false).unwrap();
            assert!(needs_password(&arch).unwrap(), "{name}");
            let dest = out.join("d");
            let e = extract(&arch, &dest, &mut |_| {}, &|| false).unwrap_err();
            assert!(is_pw(&e, PasswordError::Required), "{name}: {e:?}");
            extract_with(&arch, &dest, &pw, &mut |_| {}, &|| false).unwrap();
            assert_eq!(fs::read(dest.join("proje/n.txt")).unwrap(), b"gizli ek");
            assert_eq!(fs::read(dest.join("proje/a.txt")).unwrap(), b"merhaba dunya");
            let _ = fs::remove_dir_all(root.parent().unwrap());
            let _ = fs::remove_dir_all(&out);
        }
    }

    #[test]
    fn edit_unprotected_archive_stays_unprotected() {
        let root = make_tree();
        let out = tmp("editnopw");
        let pw = Options { password: Some("x".into()), ..Default::default() };
        for (name, format) in [("a.zip", Format::Zip), ("a.7z", Format::SevenZ)] {
            let arch = out.join(name);
            compress(&[root.clone()], &arch, format, 3, &mut |_| {}, &|| false).unwrap();
            add(&arch, &[root.join("a.txt")], "", &pw, &mut |_| {}, &|| false).unwrap();
            assert!(!needs_password(&arch).unwrap(), "{name}");
        }
        let _ = fs::remove_dir_all(root.parent().unwrap());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn edit_rejects_unsupported_formats() {
        let out = tmp("editno");
        let gz = out.join("not.txt.gz");
        {
            let mut e = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
            e.write_all(b"x").unwrap();
            e.finish().unwrap();
        }
        fs::write(out.join("r.rar"), rar::fixture::volume(&[rar::fixture::Item::File("a", b"a")], 0, false)).unwrap();
        fs::write(out.join("d.iso"), b"x").unwrap();
        fs::write(out.join("s.zip.001"), b"PK\x03\x04").unwrap();
        let src = out.join("n.txt");
        fs::write(&src, "x").unwrap();
        for f in ["not.txt.gz", "r.rar", "d.iso", "s.zip.001", "yok.docx"] {
            let p = out.join(f);
            assert!(!is_editable(&p), "{f}");
            let e = add(&p, &[src.clone()], "", &Options::default(), &mut |_| {}, &|| false).unwrap_err();
            assert!(e.to_string().contains("düzenlenemez"), "{f}: {e}");
            let e = delete(&p, &["a".into()], &Options::default(), &mut |_| {}, &|| false).unwrap_err();
            assert!(e.to_string().contains("düzenlenemez"), "{f}: {e}");
        }
        assert!(is_editable(Path::new("x.zip")) && is_editable(Path::new("x.TAR.XZ")) && is_editable(Path::new("x.7z")));
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn edit_never_adds_archive_to_itself() {
        let root = make_tree();
        let arch = root.join("kendi.zip");
        compress(&[root.join("a.txt")], &arch, Format::Zip, 3, &mut |_| {}, &|| false).unwrap();
        // adding the folder that contains the archive must skip the archive itself
        add(&arch, &[root.clone()], "", &Options::default(), &mut |_| {}, &|| false).unwrap();
        let l = paths_of(&arch, &Options::default());
        assert!(!l.iter().any(|p| p.ends_with("kendi.zip")), "{l:?}");
        assert!(l.iter().any(|p| p == "proje/sub/b.txt"), "{l:?}");
        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn formats_listed() {
        assert!(formats().contains(&"7z") && formats().contains(&"tar.zst"));
        assert!(is_supported(Path::new("a.TAR.GZ")) && !is_supported(Path::new("a.docx")));
    }
}
