//! Writes small extracted files on a pool of threads. For archives of many small files the
//! cost is creating, closing (antivirus scans each file on close) and stamping every file, not
//! decoding: a tar of 20 000 small files took 43 s written one by one, Windows' tar 20 s.
//! The archive stream is still read in order by one thread; it hands each small file's bytes
//! here (bounded by a byte budget) and keeps going.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result};

use super::unique_path;

/// Files up to this size go to the pool (bigger ones are streamed by the caller).
pub(crate) const SMALL: u64 = 4 << 20;
/// Writer threads: they mostly wait in the kernel (create / close and the antivirus scan on
/// close), so more than the cores pays off: 20 000 small files, 8 threads 26.5 s, 16 21.6 s,
/// 32 16.7 s (Windows' tar 21.7 s).
pub(crate) fn threads() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get() * 2).clamp(4, 32)
}

/// Bytes waiting in the queue at most.
const BUDGET: u64 = 64 << 20;

struct Job {
    wanted: PathBuf,
    data: Vec<u8>,
    mtime: Option<SystemTime>,
}

#[derive(Default)]
struct Shared {
    files: AtomicU64,
    bytes: AtomicU64,
    renamed: AtomicU64,
    /// The first error a writer hit; the reader stops at its next entry.
    error: Mutex<Option<anyhow::Error>>,
    failed: AtomicBool,
    /// Stop writing queued files (cancelled).
    stop: AtomicBool,
    queued: Mutex<u64>,
    room: Condvar,
    /// Folders already created.
    made: Mutex<HashSet<PathBuf>>,
}

pub(crate) struct ParWriter {
    tx: Option<SyncSender<Job>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    sh: Arc<Shared>,
}

/// Creates `wanted` (or a free variant of its name: never overwrites) with `data`.
/// Returns whether the name had to change.
pub(crate) fn write_new(wanted: &Path, data: &[u8], mtime: Option<SystemTime>, made: &Mutex<HashSet<PathBuf>>) -> Result<bool> {
    // each folder once (asking the file system again per file costs two calls through the
    // antivirus filter)
    if let Some(parent) = wanted.parent() {
        if !made.lock().unwrap().contains(parent) {
            fs::create_dir_all(parent).with_context(|| format!("Klasör oluşturulamadı: {}", parent.display()))?;
            made.lock().unwrap().insert(parent.to_path_buf());
        }
    }
    // the plain name first; only a clash looks for a free one
    let mut target = wanted.to_path_buf();
    let mut file = loop {
        match fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let next = unique_path(wanted, true);
                if next == target {
                    // unique_path saw nothing there yet create_new did: someone else's race
                    return Err(e).with_context(|| format!("Dosya oluşturulamadı: {}", target.display()));
                }
                target = next;
            }
            Err(e) => return Err(e).with_context(|| format!("Dosya oluşturulamadı: {}", target.display())),
        }
    };
    let res = file.write_all(data).with_context(|| format!("Yazılamadı: {}", target.display()));
    if res.is_ok() {
        if let Some(t) = mtime {
            let _ = file.set_modified(t);
        }
    }
    drop(file);
    if let Err(e) = res {
        let _ = fs::remove_file(&target); // partial file
        return Err(e);
    }
    Ok(target != wanted)
}

impl ParWriter {
    pub(crate) fn new(threads: usize) -> Self {
        let sh = Arc::new(Shared::default());
        let (tx, rx) = sync_channel::<Job>(1024);
        let rx = Arc::new(Mutex::new(rx));
        let threads = (0..threads.max(1))
            .map(|i| {
                let (rx, sh) = (rx.clone(), sh.clone());
                std::thread::Builder::new().name(format!("rf-unpack-write-{i}")).spawn(move || worker(&rx, &sh)).expect("writer thread")
            })
            .collect();
        Self { tx: Some(tx), threads, sh }
    }

    /// Queues a file; waits while the queue holds more than the byte budget.
    pub(crate) fn submit(&self, wanted: PathBuf, data: Vec<u8>, mtime: Option<SystemTime>) -> Result<()> {
        self.check()?;
        let n = data.len() as u64;
        {
            let mut q = self.sh.queued.lock().unwrap();
            while *q > 0 && *q + n > BUDGET && !self.sh.failed.load(Ordering::Relaxed) {
                q = self.sh.room.wait(q).unwrap();
            }
            *q += n;
        }
        self.check()?;
        if let Some(tx) = &self.tx {
            tx.send(Job { wanted, data, mtime }).map_err(|_| anyhow::anyhow!("yazıcı durdu"))?;
        }
        Ok(())
    }

    /// A writer's error, if one happened.
    pub(crate) fn check(&self) -> Result<()> {
        if self.sh.failed.load(Ordering::Relaxed) {
            if let Some(e) = self.sh.error.lock().unwrap().take() {
                return Err(e);
            }
            anyhow::bail!("Yazma hatası");
        }
        Ok(())
    }

    /// Queued files are dropped instead of written (cancel).
    pub(crate) fn abandon(&self) {
        self.sh.stop.store(true, Ordering::Relaxed);
    }

    /// Waits for the queue to drain; (files, bytes, renamed) written by the pool.
    pub(crate) fn finish(mut self) -> Result<(u64, u64, u64)> {
        self.tx.take();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.check()?;
        let sh = &self.sh;
        Ok((sh.files.load(Ordering::Relaxed), sh.bytes.load(Ordering::Relaxed), sh.renamed.load(Ordering::Relaxed)))
    }
}

impl Drop for ParWriter {
    /// An early return (error / cancel) still joins the writers: no thread outlives the call.
    fn drop(&mut self) {
        self.sh.stop.store(true, Ordering::Relaxed);
        self.tx.take();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn worker(rx: &Mutex<Receiver<Job>>, sh: &Shared) {
    loop {
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let n = job.data.len() as u64;
        if !sh.stop.load(Ordering::Relaxed) && !sh.failed.load(Ordering::Relaxed) {
            match write_new(&job.wanted, &job.data, job.mtime, &sh.made) {
                Ok(renamed) => {
                    sh.files.fetch_add(1, Ordering::Relaxed);
                    sh.bytes.fetch_add(n, Ordering::Relaxed);
                    if renamed {
                        sh.renamed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    let mut slot = sh.error.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                    sh.failed.store(true, Ordering::Relaxed);
                }
            }
        }
        drop(job);
        *sh.queued.lock().unwrap() -= n;
        sh.room.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rf-parwrite-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_everything_and_renames_clashes() {
        let d = tmp("ok");
        let pw = ParWriter::new(8);
        for i in 0..500u32 {
            pw.submit(d.join(format!("a/b{}/f{i}.txt", i % 7)), format!("içerik {i}").into_bytes(), None).unwrap();
        }
        // the same name three times: never overwritten, the later ones get "(2)", "(3)"
        for k in 0..3u8 {
            pw.submit(d.join("aynı.txt"), vec![k; 10], None).unwrap();
        }
        let (files, bytes, renamed) = pw.finish().unwrap();
        assert_eq!((files, renamed), (503, 2));
        assert!(bytes > 0);
        assert_eq!(fs::read(d.join("a/b3/f10.txt")).unwrap(), b"i\xc3\xa7erik 10");
        let mut got: Vec<Vec<u8>> = ["aynı.txt", "aynı (2).txt", "aynı (3).txt"].iter().map(|n| fs::read(d.join(n)).unwrap()).collect();
        got.sort();
        assert_eq!(got, vec![vec![0u8; 10], vec![1u8; 10], vec![2u8; 10]]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_error_reaches_the_reader() {
        let d = tmp("err");
        fs::write(d.join("dosya"), b"x").unwrap();
        let pw = ParWriter::new(4);
        // a folder can't be made where a file is
        pw.submit(d.join("dosya/içinde.txt"), b"y".to_vec(), None).unwrap();
        let err = pw.finish().unwrap_err();
        assert!(format!("{err:#}").contains("Klasör oluşturulamadı"), "{err:#}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn abandon_and_drop_do_not_hang() {
        let d = tmp("drop");
        let pw = ParWriter::new(2);
        for i in 0..200u32 {
            pw.submit(d.join(format!("f{i}")), vec![0; 1000], None).unwrap();
        }
        pw.abandon();
        drop(pw); // joins the writers
        let _ = fs::remove_dir_all(&d);
    }
}
