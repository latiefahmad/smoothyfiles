//! Our own ZIP writer: files are read, checksummed and Deflate-compressed on a pool of
//! threads; the calling thread writes the results in order (and reports progress, polls
//! cancellation). The `zip` crate compresses one file after another.
//!
//! * Memory: files up to [`IN_MEMORY`] are compressed whole in memory. Work is handed out in
//!   file order and its memory reserved in that order, so the next file to write always has
//!   its share: no deadlock, at most [`BUDGET`] (or one big file) in flight.
//! * Bigger files are compressed by the writer itself, streaming, and their local header is
//!   patched afterwards (the output is a file, so it can seek back).
//! * Format: UTF-8 names (flag bit 11), DOS time + the Unix "UT" extra field, Deflate or
//!   stored (whatever is smaller), ZIP64 where sizes, offsets or the entry count need it.
//! * No encryption here: a password goes through the `zip` crate (WinZip AES).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::DeflateEncoder;

use super::{CompressCtx, Item, timeconv};

// tests use a small threshold so the streaming path runs on small files
const IN_MEMORY: u64 = if cfg!(test) { 64 << 10 } else { 64 << 20 };
const BUDGET: u64 = 512 << 20;
const STORED: u16 = 0;
const DEFLATE: u16 = 8;

/// One file, compressed.
struct Packed {
    method: u16,
    crc: u32,
    size: u64,
    data: Vec<u8>,
}

/// What the writer needs to know about a written entry (for the central directory).
struct Written {
    name: Vec<u8>,
    method: u16,
    crc: u32,
    size: u64,
    packed: u64,
    offset: u64,
    dos: (u16, u16),
    unix: Option<u32>,
    is_dir: bool,
}

fn dos_time(t: Option<SystemTime>) -> (u16, u16) {
    match t.and_then(timeconv::systemtime_to_local_fields) {
        Some((y, mo, d, h, mi, s)) if y >= 1980 => {
            let y = (y - 1980).min(127);
            ((u16::from(h) << 11) | (u16::from(mi) << 5) | (u16::from(s) / 2), (y << 9) | (u16::from(mo) << 5) | u16::from(d))
        }
        _ => (0, (1 << 5) | 1), // 1980-01-01
    }
}

fn unix_secs(t: Option<SystemTime>) -> Option<u32> {
    t?.duration_since(SystemTime::UNIX_EPOCH).ok().and_then(|d| u32::try_from(d.as_secs()).ok())
}

/// Reads and compresses one file in memory (stored when Deflate doesn't help).
fn pack(path: &Path, level: u32) -> Result<Packed> {
    let raw = std::fs::read(path).with_context(|| format!("Dosya okunamadı: {}", path.display()))?;
    let crc = crc32fast::hash(&raw);
    let size = raw.len() as u64;
    if level == 0 || raw.is_empty() {
        return Ok(Packed { method: STORED, crc, size, data: raw });
    }
    let mut enc = DeflateEncoder::new(Vec::with_capacity(raw.len() / 2 + 64), Compression::new(level.min(9)));
    enc.write_all(&raw)?;
    let data = enc.finish()?;
    if data.len() as u64 >= size {
        Ok(Packed { method: STORED, crc, size, data: raw })
    } else {
        Ok(Packed { method: DEFLATE, crc, size, data })
    }
}

/// Hands out work in item order, with memory reserved in that order.
struct Dispatch {
    next: usize,
    reserved: u64,
}

struct Pool<'a> {
    items: &'a [Item],
    level: u32,
    dispatch: Mutex<Dispatch>,
    room: Condvar,
    done: Mutex<BTreeMap<usize, Result<Packed>>>,
    ready: Condvar,
    stop: AtomicBool,
}

impl Pool<'_> {
    fn in_memory(&self, i: usize) -> bool {
        let it = &self.items[i];
        !it.is_dir && it.size <= IN_MEMORY
    }

    fn worker(&self) {
        loop {
            // take the next in-memory file, once its memory fits (or nothing else is held)
            let i = {
                let mut d = self.dispatch.lock().unwrap();
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    while d.next < self.items.len() && !self.in_memory(d.next) {
                        d.next += 1;
                    }
                    if d.next >= self.items.len() {
                        return;
                    }
                    let need = self.items[d.next].size.max(1) * 2;
                    if d.reserved == 0 || d.reserved + need <= BUDGET {
                        d.reserved += need;
                        d.next += 1;
                        break d.next - 1;
                    }
                    d = self.room.wait(d).unwrap();
                }
            };
            let r = pack(&self.items[i].path, self.level);
            self.done.lock().unwrap().insert(i, r);
            self.ready.notify_all();
        }
    }

    fn release(&self, i: usize) {
        let need = self.items[i].size.max(1) * 2;
        let mut d = self.dispatch.lock().unwrap();
        d.reserved = d.reserved.saturating_sub(need);
        drop(d);
        self.room.notify_all();
    }

    /// Waits for item `i`'s result, polling `cancelled` meanwhile.
    fn take(&self, i: usize, cancelled: &dyn Fn() -> bool) -> Option<Result<Packed>> {
        let mut done = self.done.lock().unwrap();
        loop {
            if let Some(r) = done.remove(&i) {
                return Some(r);
            }
            if cancelled() {
                return None;
            }
            done = self.ready.wait_timeout(done, std::time::Duration::from_millis(25)).unwrap().0;
        }
    }
}

/// Counts bytes written (the output position, for header offsets).
struct Out<W: Write + Seek> {
    w: W,
    pos: u64,
}

impl<W: Write + Seek> Out<W> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
}

/// Little-endian field writer.
#[derive(Default)]
struct Le(Vec<u8>);

impl Le {
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

const U32_MAX: u64 = 0xFFFF_FFFF;

/// "UT" extra field with the modification time.
fn ut_extra(unix: Option<u32>) -> Vec<u8> {
    match unix {
        Some(t) => {
            let mut e = Le::default();
            e.u16(0x5455).u16(5).bytes(&[1]).u32(t);
            e.0
        }
        None => Vec::new(),
    }
}

/// Local header. `zip64`: sizes go in a ZIP64 extra field (0xFFFFFFFF in the header).
fn local_header(w: &Written, zip64: bool) -> Vec<u8> {
    let mut extra = ut_extra(w.unix);
    let (s32, p32) = if zip64 {
        let mut z = Le::default();
        z.u16(1).u16(16).u64(w.size).u64(w.packed);
        extra.extend_from_slice(&z.0);
        (U32_MAX as u32, U32_MAX as u32)
    } else {
        (w.size as u32, w.packed as u32)
    };
    let mut h = Le::default();
    h.u32(0x0403_4b50)
        .u16(if zip64 { 45 } else { 20 })
        .u16(1 << 11)
        .u16(w.method)
        .u16(w.dos.0)
        .u16(w.dos.1)
        .u32(w.crc)
        .u32(p32)
        .u32(s32)
        .u16(w.name.len() as u16)
        .u16(extra.len() as u16)
        .bytes(&w.name)
        .bytes(&extra);
    h.0
}

fn central_entry(w: &Written, force64: bool) -> Vec<u8> {
    let mut z = Le::default();
    let big_size = force64 || w.size >= U32_MAX;
    let big_packed = force64 || w.packed >= U32_MAX;
    let big_off = force64 || w.offset >= U32_MAX;
    if big_size {
        z.u64(w.size);
    }
    if big_packed {
        z.u64(w.packed);
    }
    if big_off {
        z.u64(w.offset);
    }
    let mut extra = ut_extra(w.unix);
    let zip64 = !z.0.is_empty();
    if zip64 {
        let mut x = Le::default();
        x.u16(1).u16(z.0.len() as u16).bytes(&z.0);
        extra.extend_from_slice(&x.0);
    }
    let pick = |big: bool, v: u64| if big { U32_MAX as u32 } else { v as u32 };
    let mut h = Le::default();
    h.u32(0x0201_4b50)
        .u16(45) // made by: MS-DOS, spec 4.5
        .u16(if zip64 { 45 } else { 20 })
        .u16(1 << 11)
        .u16(w.method)
        .u16(w.dos.0)
        .u16(w.dos.1)
        .u32(w.crc)
        .u32(pick(big_packed, w.packed))
        .u32(pick(big_size, w.size))
        .u16(w.name.len() as u16)
        .u16(extra.len() as u16)
        .u16(0)
        .u16(0)
        .u16(0)
        .u32(if w.is_dir { 0x10 } else { 0x20 })
        .u32(pick(big_off, w.offset))
        .bytes(&w.name)
        .bytes(&extra);
    h.0
}

/// Writes `items` as a ZIP to `part`. `force64` (tests) writes every ZIP64 record.
pub(crate) fn write(items: &[Item], part: &Path, level: u32, ctx: &mut CompressCtx, force64: bool) -> Result<()> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 32);
    let pool = Pool {
        items,
        level,
        dispatch: Mutex::new(Dispatch { next: 0, reserved: 0 }),
        room: Condvar::new(),
        done: Mutex::new(BTreeMap::new()),
        ready: Condvar::new(),
        stop: AtomicBool::new(false),
    };
    let file = File::create(part).with_context(|| format!("Oluşturulamadı: {}", part.display()))?;
    let mut out = Out { w: BufWriter::with_capacity(1 << 20, file), pos: 0 };
    let mut written: Vec<Written> = Vec::with_capacity(items.len());
    let res = std::thread::scope(|s| -> Result<()> {
        for _ in 0..threads {
            s.spawn(|| pool.worker());
        }
        let r = (|| -> Result<()> {
            for (i, item) in items.iter().enumerate() {
                if (ctx.cancelled)() {
                    ctx.was_cancelled = true;
                    bail!("iptal");
                }
                let name = if item.is_dir { format!("{}/", item.name.trim_end_matches('/')) } else { item.name.clone() };
                let mut w = Written {
                    name: name.into_bytes(),
                    method: STORED,
                    crc: 0,
                    size: 0,
                    packed: 0,
                    offset: out.pos,
                    dos: dos_time(item.mtime),
                    unix: unix_secs(item.mtime),
                    is_dir: item.is_dir,
                };
                ctx.current = item.name.clone();
                if item.is_dir {
                    out.put(&local_header(&w, force64))?;
                } else if pool.in_memory(i) {
                    let Some(p) = pool.take(i, ctx.cancelled) else {
                        ctx.was_cancelled = true;
                        bail!("iptal");
                    };
                    let p = p?;
                    (w.method, w.crc, w.size, w.packed) = (p.method, p.crc, p.size, p.data.len() as u64);
                    let zip64 = force64 || w.size >= U32_MAX || w.packed >= U32_MAX;
                    out.put(&local_header(&w, zip64))?;
                    out.put(&p.data).with_context(|| format!("Yazılamadı: {}", part.display()))?;
                    drop(p);
                    pool.release(i);
                    ctx.done += w.size;
                } else {
                    stream_big(&mut out, &mut w, item, level, ctx)?;
                }
                ctx.emit(false);
                written.push(w);
            }
            Ok(())
        })();
        // workers stop at their next file (after an error / cancel), or ran out of work
        pool.stop.store(true, Ordering::Relaxed);
        pool.room.notify_all();
        r
    });
    res?;
    // central directory
    let cd_start = out.pos;
    for w in &written {
        out.put(&central_entry(w, force64))?;
    }
    let cd_size = out.pos - cd_start;
    let n = written.len() as u64;
    if force64 || n >= 0xFFFF || cd_start >= U32_MAX || cd_size >= U32_MAX {
        let z64 = out.pos;
        let mut e = Le::default();
        e.u32(0x0606_4b50).u64(44).u16(45).u16(45).u32(0).u32(0).u64(n).u64(n).u64(cd_size).u64(cd_start);
        e.u32(0x0706_4b50).u32(0).u64(z64).u32(1);
        out.put(&e.0)?;
    }
    let mut e = Le::default();
    let c16 = |v: u64| if force64 || v >= 0xFFFF { 0xFFFF } else { v as u16 };
    let c32 = |v: u64| if force64 || v >= U32_MAX { U32_MAX as u32 } else { v as u32 };
    e.u32(0x0605_4b50).u16(0).u16(0).u16(c16(n)).u16(c16(n)).u32(c32(cd_size)).u32(c32(cd_start)).u16(0);
    out.put(&e.0)?;
    out.w.flush()?;
    Ok(())
}

/// A big file: Deflate-streamed by the writer, then its local header patched in place.
fn stream_big<W: Write + Seek>(out: &mut Out<W>, w: &mut Written, item: &Item, level: u32, ctx: &mut CompressCtx) -> Result<()> {
    // ZIP64 header layout from the start, so the patch has room for any size
    w.method = if level == 0 { STORED } else { DEFLATE };
    let header = local_header(w, true);
    out.put(&header)?;
    let data_start = out.pos;
    let mut src = File::open(&item.path).with_context(|| format!("Dosya açılamadı: {}", item.path.display()))?;
    let mut crc = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    let mut feed = |sink: &mut dyn Write| -> Result<()> {
        loop {
            if (ctx.cancelled)() {
                ctx.was_cancelled = true;
                bail!("iptal");
            }
            let k = src.read(&mut buf).with_context(|| format!("Okunamadı: {}", item.path.display()))?;
            if k == 0 {
                return Ok(());
            }
            crc.update(&buf[..k]);
            sink.write_all(&buf[..k])?;
            size += k as u64;
            ctx.done += k as u64;
            ctx.emit(false);
        }
    };
    if level == 0 {
        feed(&mut out.w)?;
    } else {
        let mut enc = DeflateEncoder::new(&mut out.w, Compression::new(level.min(9)));
        feed(&mut enc)?;
        enc.finish()?;
    }
    // where the encoder stopped = the end of the data
    out.w.flush()?;
    let end = out.w.seek(SeekFrom::Current(0))?;
    out.pos = end;
    (w.crc, w.size, w.packed) = (crc.finalize(), size, end - data_start);
    let patched = local_header(w, true);
    debug_assert_eq!(patched.len(), header.len());
    out.w.seek(SeekFrom::Start(w.offset))?;
    out.w.write_all(&patched)?;
    out.w.seek(SeekFrom::Start(end))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tree(root: &Path) {
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root.join("alt klasör/boş")).unwrap();
        fs::write(root.join("şiir.txt"), "Gökyüzü ışıl ışıl
".repeat(500)).unwrap();
        fs::write(root.join("boş.dat"), b"").unwrap();
        let rnd: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        fs::write(root.join("alt klasör/büyük-rastgele.bin"), &rnd).unwrap(); // > test threshold: streamed
        for i in 0..200 {
            fs::write(root.join(format!("alt klasör/f{i:03}.txt")), format!("satır {i}
").repeat(i + 1)).unwrap();
        }
    }

    fn items(root: &Path) -> Vec<Item> {
        super::super::collect(&[root.to_path_buf()], &[]).unwrap()
    }

    fn ctx<'a>(p: &'a mut dyn FnMut(super::super::Progress), c: &'a dyn Fn() -> bool) -> CompressCtx<'a> {
        CompressCtx { progress: p, cancelled: c, done: 0, total: 0, last_emit: 0, was_cancelled: false, current: String::new() }
    }

    /// Reads the archive back with the zip crate (an independent reader) and compares.
    fn check(arch: &Path, root: &Path) {
        let mut za = zip::ZipArchive::new(fs::File::open(arch).unwrap()).unwrap();
        let base = root.file_name().unwrap().to_string_lossy().into_owned();
        let mut files = 0;
        for i in 0..za.len() {
            let mut f = za.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            let on_disk = root.parent().unwrap().join(name.trim_end_matches('/'));
            if f.is_dir() {
                assert!(on_disk.is_dir(), "{name}");
                continue;
            }
            let mut got = Vec::new();
            f.read_to_end(&mut got).unwrap();
            assert_eq!(got, fs::read(&on_disk).unwrap(), "{name}");
            assert!(name.starts_with(&base));
            files += 1;
        }
        assert_eq!(files, 203);
        // and with our own reader (rarchive), through extract
        let out = arch.with_extension("out");
        let _ = fs::remove_dir_all(&out);
        super::super::extract(arch, &out, &mut |_| {}, &|| false).unwrap();
        let a = out.join(&base).join("alt klasör/büyük-rastgele.bin");
        assert_eq!(fs::read(a).unwrap(), fs::read(root.join("alt klasör/büyük-rastgele.bin")).unwrap());
        assert!(out.join(&base).join("alt klasör/boş").is_dir());
        let _ = fs::remove_dir_all(&out);
    }

    fn base(tag: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("rf-zipwrite-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&b);
        fs::create_dir_all(&b).unwrap();
        b
    }

    #[test]
    fn writes_a_zip_both_readers_accept() {
        let b = base("plain");
        let root = b.join("kaynak");
        tree(&root);
        for level in [0, 1, 6, 9] {
            let arch = b.join(format!("a{level}.zip"));
            let (mut p, c) = (|_| {}, || false);
            write(&items(&root), &arch, level, &mut ctx(&mut p, &c), false).unwrap();
            check(&arch, &root);
        }
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn zip64_records_are_readable() {
        let b = base("z64");
        let root = b.join("kaynak");
        tree(&root);
        let arch = b.join("z64.zip");
        let (mut p, c) = (|_| {}, || false);
        write(&items(&root), &arch, 6, &mut ctx(&mut p, &c), true).unwrap();
        check(&arch, &root);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn cancel_stops_cleanly() {
        let b = base("cancel");
        let root = b.join("kaynak");
        tree(&root);
        let arch = b.join("c.zip");
        let n = std::cell::Cell::new(0);
        let (mut p, c) = (|_| {}, || {
            n.set(n.get() + 1);
            n.get() > 20
        });
        let mut cx = ctx(&mut p, &c);
        assert!(write(&items(&root), &arch, 6, &mut cx, false).is_err());
        assert!(cx.was_cancelled);
        let _ = fs::remove_dir_all(&b);
    }
}
