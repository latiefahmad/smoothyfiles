//! Split archives (`name.7z.001`, `name.zip.001`, `name.001`, ...): a `Read + Seek` reader that
//! presents the numbered parts as one continuous file.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `Read + Seek` object safe alias used for archive sources (plain file or volume chain).
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// If `name` (lowercase or not) ends with `.NNN` (3+ digits), returns `(base, number)`.
pub fn split_number(name: &str) -> Option<(&str, u32)> {
    let dot = name.rfind('.')?;
    let (base, digits) = (&name[..dot], &name[dot + 1..]);
    if dot == 0 || digits.len() < 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base, digits.parse().ok()?))
}

/// Whether the file name is the first part of a numbered split set (`*.001`).
pub fn is_first_part(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).and_then(split_number).is_some_and(|(_, n)| n == 1)
}

/// `x.part07.rar` -> (`x`, 7, 2): base name, volume number and digit width.
fn rar_part_name(name: &str) -> Option<(&str, u32, usize)> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".rar")?;
    let i = stem.rfind(".part")?;
    let digits = &stem[i + 5..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..i], digits.parse().ok()?, digits.len()))
}

/// `x.r00` / `x.r01` (old-style continuation volumes) -> `x`.
fn rar_old_base(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    let ext = name[dot + 1..].to_ascii_lowercase();
    let d = ext.strip_prefix('r')?;
    (d.len() >= 2 && d.bytes().all(|b| b.is_ascii_digit())).then(|| &name[..dot])
}

/// All existing parts of the numbered set the given path belongs to, in order (`.001`, `.002`,
/// ... up to the first gap). Works from any part's path. A path that is not a numbered part
/// yields just itself.
pub fn volume_set(first: &Path) -> Vec<PathBuf> {
    let Some(name) = first.file_name().and_then(|n| n.to_str()) else { return vec![first.to_path_buf()] };
    let dir = first.parent().unwrap_or(Path::new(""));
    // RAR sets: x.part1.rar, x.part2.rar ... / x.rar, x.r00, x.r01 ...
    if let Some((base, _, width)) = rar_part_name(name) {
        let mut out = Vec::new();
        for i in 1..100_000u32 {
            let p = dir.join(format!("{base}.part{i:0width$}.rar"));
            if !p.is_file() {
                break;
            }
            out.push(p);
        }
        return if out.is_empty() { vec![first.to_path_buf()] } else { out };
    }
    let old_base = rar_old_base(name).or_else(|| name.to_ascii_lowercase().ends_with(".rar").then(|| &name[..name.len() - 4]));
    if let Some(base) = old_base {
        let mut out = vec![dir.join(format!("{base}.rar"))];
        'outer: for letter in ['r', 's'] {
            for i in 0..100u32 {
                let p = dir.join(format!("{base}.{letter}{i:02}"));
                if !p.is_file() {
                    break 'outer;
                }
                out.push(p);
            }
        }
        return out;
    }
    let Some((base, num)) = split_number(name) else {
        return vec![first.to_path_buf()];
    };
    let width = name.len() - base.len() - 1;
    let mut out = Vec::new();
    for i in 1..100_000u32 {
        let p = dir.join(format!("{base}.{i:0width$}"));
        if p.is_file() {
            out.push(p);
        } else {
            break;
        }
    }
    if out.is_empty() {
        // e.g. only ".002" exists; fall back to what was asked for
        let _ = num;
        out.push(first.to_path_buf());
    }
    out
}

/// Chains several files into one seekable reader.
pub struct VolumeReader {
    parts: Vec<(File, u64)>,
    /// Absolute start offset of each part.
    starts: Vec<u64>,
    total: u64,
    pos: u64,
}

impl VolumeReader {
    pub fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut starts = Vec::with_capacity(paths.len());
        let mut total = 0u64;
        for p in paths {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            starts.push(total);
            total += len;
            parts.push((f, len));
        }
        Ok(VolumeReader { parts, starts, total, pos: 0 })
    }
}

impl Read for VolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        // Part containing `pos` (the last start <= pos; empty parts are skipped naturally).
        let idx = self.starts.partition_point(|&s| s <= self.pos) - 1;
        let (file, len) = &mut self.parts[idx];
        let off = self.pos - self.starts[idx];
        let want = buf.len().min((*len - off) as usize);
        if want == 0 {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(off))?;
        let n = file.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for VolumeReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.total as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negatif konum"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

/// Opens a plain file, or the chained parts when `path` is a `.001` part with siblings.
pub fn open_source(path: &Path) -> io::Result<Box<dyn ReadSeek>> {
    if is_first_part(path) {
        let set = volume_set(path);
        return Ok(Box::new(VolumeReader::open(&set)?));
    }
    Ok(Box::new(File::open(path)?))
}

/// Total size of the archive on disk (sum of all parts for a split set).
pub fn source_len(path: &Path) -> io::Result<u64> {
    if is_first_part(path) {
        let mut t = 0;
        for p in volume_set(path) {
            t += std::fs::metadata(p)?.len();
        }
        return Ok(t);
    }
    Ok(std::fs::metadata(path)?.len())
}
