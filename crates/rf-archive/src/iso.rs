//! ISO 9660 disc images (read-only) through `hadris-iso`: Rock Ridge, Joliet (long Unicode
//! names), the ISO 9660:1999 tree and plain ISO 9660 are all understood; the most capable tree
//! of an image is shown (Rock Ridge > Joliet > primary). Version suffixes (`;1`) are hidden by
//! the reader. Multi-extent files are supported by the reader.
//!
//! UDF (1.02 - 2.01, `hadris-udf`) is tried first, so UDF-only images and the UDF half of
//! "bridge" discs (Windows installers, DVDs) show their real content; images without UDF fall
//! back to ISO 9660. Not covered: UDF 2.50+ metadata partitions / virtual partitions
//! (Blu-ray, multi-session rewritable media).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use hadris_fs::sync::FileSystem;
use hadris_fs::{DirCursor, FileType, MountOptions, NodeId, OpenMode};
use hadris_io::StdIo;
use hadris_iso::sync::IsoFs;
use hadris_storage::BlockSize;
use hadris_udf::sync::UdfFs;
use hadris_storage::ReadOnly;
use hadris_storage::sync::StreamDevice;

use super::volume::{ReadSeek, open_source};
use super::{ArchiveEntry, ExtractStats, Extractor, Options, Progress, sanitize};

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 2_000_000;

type Device = StreamDevice<ReadOnly<StdIo<Box<dyn ReadSeek>>>>;

struct Node {
    path: String,
    id: NodeId,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
}

fn device(path: &Path) -> Result<Device> {
    let src = open_source(path).with_context(|| format!("Arşiv açılamadı: {}", path.display()))?;
    StreamDevice::new(ReadOnly::new(StdIo::new(src)), BlockSize::new(2048).unwrap())
        .map_err(|e| anyhow!("ISO okunamadı: {e:?}"))
}

fn open_iso(path: &Path) -> Result<IsoFs<Device>> {
    IsoFs::mount(device(path)?, MountOptions::new()).map_err(|e| anyhow!("ISO okunamadı (bozuk olabilir): {e}"))
}

fn open_udf(path: &Path) -> Result<UdfFs<Device>> {
    UdfFs::mount(device(path)?, MountOptions::new()).map_err(|e| anyhow!("UDF okunamadı: {e}"))
}

/// Walks the whole tree (depth-first, directories before their children); links and special
/// files are left out.
fn walk<F: FileSystem>(fs: &mut F) -> Result<Vec<Node>> {
    let mut out = Vec::new();
    let mut stack: Vec<(NodeId, String, usize)> = vec![(fs.root(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        if depth >= MAX_DEPTH {
            bail!("ISO klasör ağacı çok derin");
        }
        let mut children = Vec::new();
        let mut cursor = DirCursor::START;
        while let Some(e) = fs.readdir(dir, cursor).map_err(|e| anyhow!("ISO klasörü okunamadı: {e}"))? {
            cursor = e.next_cursor();
            let name = String::from_utf8_lossy(e.name().as_bytes()).into_owned();
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            let meta = e.metadata();
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let mtime = meta.modified().map(|d| {
                let s = d.unix_seconds();
                if s >= 0 { UNIX_EPOCH + Duration::from_secs(s as u64) } else { UNIX_EPOCH - Duration::from_secs(s.unsigned_abs()) }
            });
            match meta.file_type() {
                FileType::Dir => children.push(Node { path, id: e.node(), is_dir: true, size: 0, mtime }),
                FileType::File => children.push(Node { path, id: e.node(), is_dir: false, size: meta.len(), mtime }),
                _ => {}
            }
            if out.len() + children.len() > MAX_ENTRIES {
                bail!("ISO çok fazla girdi içeriyor");
            }
        }
        // Emit in directory order; descend afterwards (stack keeps listing order).
        let mut dirs = Vec::new();
        for c in children {
            if c.is_dir {
                dirs.push((c.id, c.path.clone(), depth + 1));
            }
            out.push(c);
        }
        stack.extend(dirs.into_iter().rev());
    }
    Ok(out)
}

fn entries(nodes: Vec<Node>) -> Vec<ArchiveEntry> {
    nodes
        .into_iter()
        .map(|n| ArchiveEntry {
            path: n.path,
            size: n.size,
            packed: 0,
            is_dir: n.is_dir,
            modified_ms: n.mtime.map_or(0, super::mtime_ms),
            encrypted: false,
        })
        .collect()
}

pub fn list(path: &Path, _opts: &Options) -> Result<Vec<ArchiveEntry>> {
    if let Ok(mut udf) = open_udf(path) {
        return Ok(entries(walk(&mut udf)?));
    }
    Ok(entries(walk(&mut open_iso(path)?)?))
}

/// Adapts an open ISO node to `std::io::Read`.
struct NodeReader<'a, F: FileSystem> {
    fs: &'a mut F,
    node: NodeId,
    off: u64,
    size: u64,
}

impl<F: FileSystem> std::io::Read for NodeReader<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.off >= self.size {
            return Ok(0);
        }
        let n = self.fs.read(self.node, self.off, buf).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.off += n as u64;
        Ok(n)
    }
}

pub fn extract(
    path: &Path,
    dest: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    if let Ok(mut udf) = open_udf(path) {
        return extract_fs(&mut udf, dest, &crate::Only::new(&opts.only), progress, cancelled);
    }
    extract_fs(&mut open_iso(path)?, dest, &crate::Only::new(&opts.only), progress, cancelled)
}

fn extract_fs<F: FileSystem>(
    fs: &mut F,
    dest: &Path,
    only: &crate::Only,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    let nodes = walk(fs)?;
    let mut total = 0u64;
    for n in &nodes {
        sanitize(&n.path)?;
        total += n.size;
    }
    let mut ex = Extractor::new(dest, total, None, progress, cancelled).only(only.clone());
    for n in &nodes {
        let Some(rel) = sanitize(&n.path)? else { continue };
        if n.is_dir {
            ex.dir(&rel, &n.path)?;
            continue;
        }
        if !only.wants(&rel) {
            continue;
        }
        fs.open(n.id, OpenMode::Read).map_err(|e| anyhow!("ISO dosyası açılamadı: {e}"))?;
        let res = ex.file(&rel, &n.path, &mut NodeReader { fs: &mut *fs, node: n.id, off: 0, size: n.size }, n.mtime);
        let _ = fs.close(n.id);
        res?;
    }
    ex.emit("", true);
    Ok(ex.stats)
}
