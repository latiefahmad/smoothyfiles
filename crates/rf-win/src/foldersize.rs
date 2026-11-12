//! Folder size calculation: the details view's Size column ("Boyutu hesapla" / Settings →
//! "Klasör boyutlarını hesapla"), the preview pane and Properties.
//!
//! Directories are listed with `FindFirstFileExW` (basic info, large fetch) by a few shared
//! worker threads at below-normal CPU priority (the UI thread always wins; I/O priority stays
//! normal, background mode throttles the disk so hard that a column of folders crawls).
//! The workers serve all requested folders **round-robin**, one directory at a time: a small
//! folder clicked while a huge one is being measured still finishes in milliseconds instead of
//! queueing behind it. Reparse points (junctions, symlinks, OneDrive placeholders) are neither
//! counted nor followed. Unreadable sub-folders are skipped silently.
//!
//! Hard links are counted once per link; compressed / sparse files are measured by logical size.
//!
//! The shared cache can be kept in a file between runs ([`FolderSizeCache::persist`]). A stored
//! value is only a first guess: a folder's own date does not move when something deep inside it
//! changes, so values read from the file are *provisional*. They are shown at once
//! ([`SizeState::Provisional`]) and every provisional folder is measured again when it is asked
//! for; the fresh result replaces the guess.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use parking_lot::{Condvar, Mutex};
use rf_core::attrs;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
use windows::core::PCWSTR;

pub use crate::hash::Cancelled;
use crate::search::for_each_raw;
use crate::wide::{from_wide, wide};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderSize {
    /// Sum of logical file sizes.
    pub bytes: u64,
    pub files: u64,
    /// Sub-folders (the root itself is not counted).
    pub dirs: u64,
    /// Approximate size on disk: every file rounded up to the volume cluster size. `None` when
    /// the cluster size could not be determined.
    pub allocated: Option<u64>,
}

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    allocated: AtomicU64,
}

impl Counters {
    fn snapshot(&self, track_alloc: bool) -> FolderSize {
        FolderSize {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            allocated: track_alloc.then(|| self.allocated.load(Ordering::Relaxed)),
        }
    }
}

/// Bytes per cluster of the volume holding `dir`.
pub(crate) fn cluster_size(dir: &Path) -> Option<u64> {
    let src = wide(dir);
    let mut root = [0u16; 261];
    unsafe {
        GetVolumePathNameW(PCWSTR(src.as_ptr()), &mut root).ok()?;
        let (mut spc, mut bps) = (0u32, 0u32);
        GetDiskFreeSpaceW(PCWSTR(root.as_ptr()), Some(&mut spc), Some(&mut bps), None, None).ok()?;
        let c = spc as u64 * bps as u64;
        (c > 0).then_some(c)
    }
}

/// Folder sizes shared by the Size column, the preview pane and Properties.
pub fn shared_cache() -> &'static FolderSizeCache {
    static CACHE: OnceLock<FolderSizeCache> = OnceLock::new();
    CACHE.get_or_init(FolderSizeCache::default)
}

// ------------------------------------------------------------------------- scheduler

/// One folder being measured.
struct Job {
    batch: Arc<SizeBatch>,
    i: usize,
    root: PathBuf,
    /// Directories still to list (depth-first: the most recently found first).
    todo: Mutex<Vec<PathBuf>>,
    started: AtomicBool,
    /// The disk's limit on directories listed at once (`None`: not limited). Looked up by the
    /// worker that lists the folder itself, never on the caller's thread.
    lane: OnceLock<Option<Arc<Lane>>>,
}

/// Spinning disks get this many directories listed at once: with eight readers the head spends
/// its time seeking between them and the whole walk gets slower, not faster.
const HDD_LANES: usize = 2;

/// One disk's count of directories being listed, and its limit.
struct Lane {
    busy: AtomicUsize,
    cap: usize,
}

/// The lane for the disk holding `root`: spinning disks (seek penalty) are limited, SSDs,
/// network paths and disks that don't say are not. Asked once per drive letter; the answers
/// are forgotten when a drive comes or goes (a USB disk may take a letter an SSD had).
fn lane_for(root: &Path) -> Option<Arc<Lane>> {
    static LANES: OnceLock<Mutex<(u32, HashMap<u8, Option<Arc<Lane>>>)>> = OnceLock::new();
    let b = root.to_str()?.as_bytes();
    if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
        return None;
    }
    let letter = b[0].to_ascii_uppercase();
    let mask = crate::drives::drive_mask();
    let mut lanes = LANES.get_or_init(Default::default).lock();
    if lanes.0 != mask {
        *lanes = (mask, HashMap::new());
    }
    lanes
        .1
        .entry(letter)
        .or_insert_with(|| {
            let hdd = crate::drives::seek_penalty(root) == Some(true);
            log::debug!("foldersize: drive {}: {}", letter as char, if hdd { "spinning, 2 at once" } else { "not limited" });
            hdd.then(|| Arc::new(Lane { busy: AtomicUsize::new(0), cap: HDD_LANES }))
        })
        .clone()
}

struct Sched {
    jobs: Mutex<Vec<Arc<Job>>>,
    cv: Condvar,
    next: AtomicUsize,
}

/// Measured on NVMe with 16 hardware threads: 8 workers is the sweet spot, more only contend.
fn sched() -> &'static Sched {
    static S: OnceLock<&'static Sched> = OnceLock::new();
    S.get_or_init(|| {
        let s: &'static Sched = Box::leak(Box::new(Sched { jobs: Mutex::new(Vec::new()), cv: Condvar::new(), next: AtomicUsize::new(0) }));
        let n = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 8);
        for k in 0..n {
            let _ = std::thread::Builder::new().name(format!("rf-dirsize-{k}")).spawn(move || {
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                }
                worker(s)
            });
        }
        s
    })
}

/// What a worker took: the folder's job, the directory to list, and the lane it holds a place
/// in (to give back when done).
type Taken = (Arc<Job>, PathBuf, Option<Arc<Lane>>);

/// Next directory from `jobs`, starting at `start` and taking the jobs in turn so every folder
/// keeps moving; jobs whose disk already has its lanes full are passed over. The caller holds
/// the jobs lock.
fn pick(jobs: &[Arc<Job>], start: usize) -> Option<Taken> {
    let n = jobs.len();
    for k in 0..n {
        let job = &jobs[(start + k) % n];
        let lane = job.lane.get().cloned().flatten();
        if let Some(l) = &lane {
            if l.busy.load(Ordering::Acquire) >= l.cap {
                continue;
            }
        }
        if let Some(dir) = job.todo.lock().pop() {
            if let Some(l) = &lane {
                l.busy.fetch_add(1, Ordering::AcqRel);
            }
            return Some((job.clone(), dir, lane));
        }
    }
    None
}

/// Next directory to list; waits while there is none.
fn take(s: &Sched) -> Taken {
    let mut jobs = s.jobs.lock();
    loop {
        if !jobs.is_empty() {
            let start = s.next.fetch_add(1, Ordering::Relaxed);
            if let Some(got) = pick(&jobs, start) {
                return got;
            }
        }
        if jobs.is_empty() {
            // nothing to do: sleep until a batch comes (start() notifies); no idle wake-ups
            s.cv.wait(&mut jobs);
        } else {
            // work exists but its disk is busy with other workers: look again soon
            s.cv.wait_for(&mut jobs, Duration::from_millis(250));
        }
    }
}

fn worker(s: &'static Sched) {
    loop {
        let (job, dir, lane) = take(s);
        process(s, &job, dir);
        if let Some(l) = lane {
            l.busy.fetch_sub(1, Ordering::AcqRel);
            // a worker may be waiting for this disk's lane
            drop(s.jobs.lock());
            s.cv.notify_one();
        }
    }
}

fn finish(s: &Sched, job: &Job) {
    let slot = &job.batch.slots[job.i];
    if !job.batch.stop.load(Ordering::Relaxed) {
        let size = slot.c.snapshot(slot.cluster.get().copied().flatten().is_some());
        shared_cache().insert_at(&job.root, size, slot.mtime.get().copied().flatten());
    }
    slot.done.store(true, Ordering::Release);
    s.jobs.lock().retain(|j| !(Arc::ptr_eq(&j.batch, &job.batch) && j.i == job.i));
}

fn process(s: &Sched, job: &Job, dir: PathBuf) {
    let slot = &job.batch.slots[job.i];
    if job.batch.stop.load(Ordering::Relaxed) {
        // drop the rest of this folder's work at once
        let left = job.todo.lock().drain(..).count();
        if slot.pending.fetch_sub(left + 1, Ordering::AcqRel) == left + 1 {
            finish(s, job);
        }
        return;
    }
    if !job.started.swap(true, Ordering::Relaxed) {
        if let Some(c) = shared_cache().get(&job.root) {
            slot.c.bytes.store(c.bytes, Ordering::Relaxed);
            slot.c.files.store(c.files, Ordering::Relaxed);
            slot.c.dirs.store(c.dirs, Ordering::Relaxed);
            slot.c.allocated.store(c.allocated.unwrap_or(0), Ordering::Relaxed);
            let _ = slot.cluster.set(c.allocated.map(|_| 1));
            let _ = slot.mtime.set(dir_mtime(&job.root));
            slot.pending.store(0, Ordering::Release);
            finish(s, job);
            return;
        }
        // mtime before walking: a change during the walk then invalidates the entry
        let _ = slot.mtime.set(dir_mtime(&job.root));
        // from now on the folder's sub-directories share its disk's lanes
        job.lane.get_or_init(|| lane_for(&job.root));
        let _ = slot.cluster.set(cluster_size(&job.root));
    }
    let cluster = slot.cluster.get().copied().flatten();
    let (mut bytes, mut files, mut alloc) = (0u64, 0u64, 0u64);
    let mut subdirs = Vec::new();
    let r = for_each_raw(&dir, |d| {
        let n = &d.cFileName;
        if n[0] == b'.' as u16 && (n[1] == 0 || (n[1] == b'.' as u16 && n[2] == 0)) {
            return;
        }
        let a = d.dwFileAttributes;
        if a & attrs::REPARSE_POINT != 0 {
            return;
        }
        if a & attrs::DIRECTORY != 0 {
            subdirs.push(dir.join(from_wide(n)));
        } else {
            let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
            bytes += size;
            files += 1;
            if let Some(cl) = cluster {
                alloc += size.div_ceil(cl) * cl;
            }
        }
    });
    if let Err(code) = r {
        log::debug!("foldersize: {} failed ({code})", dir.display());
    }
    slot.c.bytes.fetch_add(bytes, Ordering::Relaxed);
    slot.c.files.fetch_add(files, Ordering::Relaxed);
    slot.c.dirs.fetch_add(subdirs.len() as u64, Ordering::Relaxed);
    slot.c.allocated.fetch_add(alloc, Ordering::Relaxed);
    if !subdirs.is_empty() {
        slot.pending.fetch_add(subdirs.len(), Ordering::AcqRel);
        let n = subdirs.len();
        job.todo.lock().extend(subdirs);
        // wake idle workers to share this folder's new work (taking the lock first: a worker
        // that just found nothing is then already waiting and gets the signal)
        drop(s.jobs.lock());
        for _ in 0..n.min(8) {
            s.cv.notify_one();
        }
    }
    if slot.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
        finish(s, job);
    }
}

// ------------------------------------------------------------------------- batch

#[derive(Default)]
struct Slot {
    c: Counters,
    /// Directories queued or being listed for this folder.
    pending: AtomicUsize,
    done: AtomicBool,
    cluster: OnceLock<Option<u64>>,
    mtime: OnceLock<Option<SystemTime>>,
    /// Value remembered from an earlier run, shown until the new measurement ends.
    seed: Option<FolderSize>,
}

/// State of one folder in a [`SizeBatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeState {
    /// Still walking; bytes counted so far.
    Partial(u64),
    /// Still being measured again, but an earlier run's result is known: show it and let the
    /// fresh one replace it silently.
    Provisional(FolderSize),
    Done(FolderSize),
}

/// Sizes of several folders, measured together by the shared workers. Each folder finishes on
/// its own; results also land in [`shared_cache`] (a folder measured before and unchanged since
/// comes straight from there).
pub struct SizeBatch {
    slots: Vec<Slot>,
    stop: AtomicBool,
}

impl SizeBatch {
    /// Starts at once and returns immediately (nothing touches the disk on the calling thread).
    pub fn start(dirs: Vec<PathBuf>) -> Arc<Self> {
        let b = Arc::new(SizeBatch { slots: dirs.iter().map(|d| Slot { seed: shared_cache().provisional(d), ..Default::default() }).collect(), stop: AtomicBool::new(false) });
        let s = sched();
        let mut jobs = s.jobs.lock();
        for (i, dir) in dirs.into_iter().enumerate() {
            b.slots[i].pending.store(1, Ordering::Relaxed);
            jobs.push(Arc::new(Job { batch: b.clone(), i, root: dir.clone(), todo: Mutex::new(vec![dir]), started: AtomicBool::new(false), lane: OnceLock::new() }));
        }
        drop(jobs);
        s.cv.notify_all();
        b
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn state(&self, i: usize) -> SizeState {
        let s = &self.slots[i];
        if s.done.load(Ordering::Acquire) {
            SizeState::Done(s.c.snapshot(s.cluster.get().copied().flatten().is_some()))
        } else if let Some(seed) = s.seed {
            SizeState::Provisional(seed)
        } else {
            SizeState::Partial(s.c.bytes.load(Ordering::Relaxed))
        }
    }

    /// Running totals (final once [`finished`](Self::finished)).
    pub fn totals(&self, i: usize) -> FolderSize {
        let s = &self.slots[i];
        s.c.snapshot(s.cluster.get().copied().flatten().is_some())
    }

    pub fn finished(&self) -> bool {
        self.slots.iter().all(|s| s.done.load(Ordering::Acquire))
    }

    /// Stops all walks soon; results are then not cached.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Totals for everything under `dir` (blocking: call off the UI thread). `progress` receives
/// running totals about every 100 ms on the calling thread, `cancelled` is polled there too; on
/// cancel the error downcasts to [`Cancelled`]. Not cached here: see [`FolderSizeCache`].
pub fn folder_size(
    dir: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&FolderSize),
) -> anyhow::Result<FolderSize> {
    anyhow::ensure!(dir.is_dir(), "Klasör bulunamadı: {}", dir.display());
    // measure afresh (Properties / preview ask for the truth, the cache may be stale)
    shared_cache().invalidate(dir);
    let b = SizeBatch::start(vec![dir.to_path_buf()]);
    let mut since = std::time::Instant::now();
    while !b.finished() {
        if cancelled() {
            b.cancel();
            return Err(Cancelled.into());
        }
        std::thread::sleep(Duration::from_millis(5));
        if since.elapsed() >= Duration::from_millis(100) {
            since = std::time::Instant::now();
            progress(&b.totals(0));
        }
    }
    let total = b.totals(0);
    progress(&total);
    Ok(total)
}

// ------------------------------------------------------------------------- cache

struct CacheEntry {
    /// Last-write time of the root folder when computed.
    mtime: Option<SystemTime>,
    size: FolderSize,
    seq: u64,
    /// Read from the cache file, not measured in this run: good for showing, not for trusting.
    provisional: bool,
}

/// Folders kept in the cache file (the most recently used ones).
pub const PERSIST_LIMIT: usize = 20_000;

/// How long after a change the cache file is written; results arrive in bursts, so one write
/// covers many of them.
const SAVE_DELAY: Duration = Duration::from_secs(3);

/// Concurrent cache of computed folder sizes, keyed by path (case-insensitive) and validated
/// against the root folder's last-write time. The root's mtime changes when entries are added,
/// removed or renamed directly inside it, **not** when something deeper changes, so deep edits
/// show up after [`invalidate`](Self::invalidate) (e.g. from the file watcher) or a refresh.
pub struct FolderSizeCache {
    map: Mutex<HashMap<String, CacheEntry>>,
    seq: AtomicU64,
    capacity: usize,
    /// Something changed since the cache file was last written.
    dirty: Mutex<bool>,
    dirty_cv: Condvar,
}

impl Default for FolderSizeCache {
    fn default() -> Self {
        Self::new(PERSIST_LIMIT)
    }
}

fn key(dir: &Path) -> String {
    dir.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

fn dir_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
}

impl FolderSizeCache {
    pub fn new(capacity: usize) -> Self {
        Self { map: Mutex::new(HashMap::new()), seq: AtomicU64::new(1), capacity: capacity.max(16), dirty: Mutex::new(false), dirty_cv: Condvar::new() }
    }

    fn mark_dirty(&self) {
        *self.dirty.lock() = true;
        self.dirty_cv.notify_one();
    }

    /// Cached size if it was measured in this run and the folder's mtime is unchanged. Cheap
    /// (one `stat`). Values remembered from an earlier run never answer here: see
    /// [`provisional`](Self::provisional).
    pub fn get(&self, dir: &Path) -> Option<FolderSize> {
        let mtime = dir_mtime(dir)?;
        let map = self.map.lock();
        let e = map.get(&key(dir))?;
        (!e.provisional && e.mtime == Some(mtime)).then_some(e.size)
    }

    /// The value remembered from an earlier run, if that is all there is. No disk access.
    pub fn provisional(&self, dir: &Path) -> Option<FolderSize> {
        let map = self.map.lock();
        map.get(&key(dir)).filter(|e| e.provisional).map(|e| e.size)
    }

    pub fn insert(&self, dir: &Path, size: FolderSize) {
        self.insert_at(dir, size, dir_mtime(dir));
    }

    fn insert_at(&self, dir: &Path, size: FolderSize, mtime: Option<SystemTime>) {
        {
            let mut map = self.map.lock();
            if map.len() >= self.capacity {
                // Evict the older half.
                let mut seqs: Vec<u64> = map.values().map(|e| e.seq).collect();
                seqs.sort_unstable();
                let cut = seqs[seqs.len() / 2];
                map.retain(|_, e| e.seq > cut);
            }
            map.insert(key(dir), CacheEntry { mtime, size, seq: self.seq.fetch_add(1, Ordering::Relaxed), provisional: false });
        }
        self.mark_dirty();
    }

    /// Forgets `dir` and every cached folder below it.
    pub fn invalidate(&self, dir: &Path) {
        let k = key(dir);
        let prefix = format!("{k}\\");
        self.map.lock().retain(|p, _| *p != k && !p.starts_with(&prefix));
        self.mark_dirty();
    }

    pub fn clear(&self) {
        self.map.lock().clear();
        self.mark_dirty();
    }

    /// Returns the cached value, or computes (blocking, see [`folder_size`]) and stores it.
    /// Cancelled or failed computations are not cached.
    pub fn get_or_compute(
        &self,
        dir: &Path,
        cancelled: &dyn Fn() -> bool,
        progress: &mut dyn FnMut(&FolderSize),
    ) -> anyhow::Result<FolderSize> {
        if let Some(s) = self.get(dir) {
            progress(&s);
            return Ok(s);
        }
        // Read the mtime *before* walking: a change during the walk then invalidates the entry.
        let before = dir_mtime(dir);
        let size = folder_size(dir, cancelled, progress)?;
        self.insert_at(dir, size, before);
        Ok(size)
    }

    // --------------------------------------------------------------------- cache file

    /// Writes the `limit` most recently used folders to `path`: into a temporary file next to
    /// it first, then renamed over it, so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path, limit: usize) -> std::io::Result<()> {
        let mut rows: Vec<(u64, String, FolderSize)> = {
            let map = self.map.lock();
            map.iter().map(|(k, e)| (e.seq, k.clone(), e.size)).collect()
        };
        rows.sort_unstable_by_key(|r| r.0);
        let skip = rows.len().saturating_sub(limit);
        let rows = &rows[skip..];
        let mut buf = Vec::with_capacity(16 + rows.len() * 64);
        buf.extend_from_slice(FILE_MAGIC);
        buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        // oldest first: reading back in order keeps the recency
        for (_, k, s) in rows {
            let kb = k.as_bytes();
            let n = kb.len().min(u16::MAX as usize);
            buf.extend_from_slice(&(n as u16).to_le_bytes());
            buf.extend_from_slice(&kb[..n]);
            for v in [s.bytes, s.files, s.dirs, s.allocated.unwrap_or(NO_ALLOC)] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        let sum = fnv1a(&buf);
        buf.extend_from_slice(&sum.to_le_bytes());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Reads a cache file written by [`save_to`](Self::save_to); the values enter as provisional
    /// and never replace anything measured in this run. A missing, truncated, corrupt or
    /// foreign file is ignored. Returns how many folders were taken.
    pub fn load_from(&self, path: &Path) -> usize {
        let Ok(data) = std::fs::read(path) else { return 0 };
        let Some(rows) = parse_file(&data) else {
            log::debug!("foldersize: ignoring unreadable cache file {}", path.display());
            return 0;
        };
        let mut map = self.map.lock();
        let mut taken = 0;
        // rows are oldest first: when there are more than fit, the newest ones are kept
        let skip = rows.len().saturating_sub(self.capacity.saturating_sub(map.len()));
        for (k, size) in rows.into_iter().skip(skip) {
            if let std::collections::hash_map::Entry::Vacant(v) = map.entry(k) {
                // below everything measured in this run (those start at 1), so the next
                // eviction takes these first
                v.insert(CacheEntry { mtime: None, size, seq: 0, provisional: true });
                taken += 1;
            }
        }
        taken
    }

    /// Keeps this cache in the file `path`: reads it now (on a background thread), then writes
    /// it again a few seconds after each change, off the calling thread.
    pub fn persist(&'static self, path: PathBuf) {
        self.persist_with(path, SAVE_DELAY, PERSIST_LIMIT);
    }

    fn persist_with(&'static self, path: PathBuf, delay: Duration, limit: usize) {
        let _ = std::thread::Builder::new().name("rf-dirsize-save".into()).spawn(move || {
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            let n = self.load_from(&path);
            log::debug!("foldersize: {n} remembered sizes from {}", path.display());
            loop {
                {
                    let mut d = self.dirty.lock();
                    while !*d {
                        self.dirty_cv.wait(&mut d);
                    }
                }
                std::thread::sleep(delay);
                *self.dirty.lock() = false;
                if let Err(e) = self.save_to(&path, limit) {
                    log::debug!("foldersize: could not write {}: {e}", path.display());
                }
            }
        });
    }
}

const FILE_MAGIC: &[u8; 8] = b"RFSIZE\x00\x01";
/// Stored for "size on disk unknown".
const NO_ALLOC: u64 = u64::MAX;

fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
}

/// The rows of a cache file, or `None` when it is not exactly what `save_to` writes.
fn parse_file(data: &[u8]) -> Option<Vec<(String, FolderSize)>> {
    let body_len = data.len().checked_sub(8)?;
    let (body, sum) = data.split_at(body_len);
    if u64::from_le_bytes(sum.try_into().ok()?) != fnv1a(body) || body.get(..8)? != FILE_MAGIC {
        return None;
    }
    let count = u32::from_le_bytes(body.get(8..12)?.try_into().ok()?) as usize;
    let mut at: usize = 12;
    let mut take = |n: usize| -> Option<&[u8]> {
        let r = body.get(at..at.checked_add(n)?)?;
        at += n;
        Some(r)
    };
    // every row is at least 34 bytes: a huge count in a short file is rejected before allocating
    let mut rows = Vec::with_capacity(count.min(body.len() / 34));
    for _ in 0..count {
        let n = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
        let k = String::from_utf8(take(n)?.to_vec()).ok()?;
        let mut v = [0u64; 4];
        for x in &mut v {
            *x = u64::from_le_bytes(take(8)?.try_into().ok()?);
        }
        rows.push((k, FolderSize { bytes: v[0], files: v[1], dirs: v[2], allocated: (v[3] != NO_ALLOC).then_some(v[3]) }));
    }
    (at == body.len()).then_some(rows)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// A full lane passes its disk's work over (other disks keep going); a free place is taken
    /// and counted, and the lane is handed back with the work.
    #[test]
    fn full_lanes_are_passed_over() {
        let b = Arc::new(SizeBatch { slots: vec![Slot::default(), Slot::default()], stop: AtomicBool::new(false) });
        let job = |i: usize, dirs: &[&str], lane: Option<Arc<Lane>>| {
            let j = Job {
                batch: b.clone(),
                i,
                root: PathBuf::from(dirs[0]),
                todo: Mutex::new(dirs.iter().map(PathBuf::from).collect()),
                started: AtomicBool::new(true),
                lane: OnceLock::new(),
            };
            let _ = j.lane.set(lane);
            Arc::new(j)
        };
        let hdd = Arc::new(Lane { busy: AtomicUsize::new(0), cap: 2 });
        let jobs = vec![job(0, &["D:\\a", "D:\\a\\1", "D:\\a\\2"], Some(hdd.clone())), job(1, &["C:\\b", "C:\\b\\1"], None)];
        // two places on the spinning disk
        let (_, d1, l1) = pick(&jobs, 0).unwrap();
        let (_, d2, l2) = pick(&jobs, 0).unwrap();
        assert!(d1.starts_with("D:\\a") && d2.starts_with("D:\\a"));
        assert!(l1.is_some() && l2.is_some());
        assert_eq!(hdd.busy.load(Ordering::Relaxed), 2);
        // full: the next pick goes to the other disk even though the round starts at the HDD job
        let (j, d3, l3) = pick(&jobs, 0).unwrap();
        assert_eq!(j.i, 1);
        assert!(d3.starts_with("C:\\b") && l3.is_none());
        // one place handed back: the HDD's last directory can go
        hdd.busy.fetch_sub(1, Ordering::AcqRel);
        let (j, _, _) = pick(&jobs, 0).unwrap();
        assert_eq!(j.i, 0);
    }

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rf-fsize-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("x.bin"), vec![1u8; 1000]).unwrap();
        std::fs::write(root.join("a/y.bin"), vec![2u8; 5000]).unwrap();
        std::fs::write(root.join("a/b/z.bin"), vec![3u8; 70_000]).unwrap();
        root
    }

    #[test]
    fn sums_tree() {
        let root = tree("sum");
        let s = folder_size(&root, &|| false, &mut |_| {}).unwrap();
        assert_eq!((s.bytes, s.files, s.dirs), (76_000, 3, 3));
        let alloc = s.allocated.expect("cluster size");
        assert!(alloc >= s.bytes && alloc < s.bytes + 3 * 64 * 1024);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn skips_reparse_points() {
        let root = tree("junc");
        let target = tree("junc-target");
        // Directory junction (no admin needed) via mklink /J.
        let link = root.join("link");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let s = folder_size(&root, &|| false, &mut |_| {}).unwrap();
        assert_eq!((s.bytes, s.files, s.dirs), (76_000, 3, 3), "junction was followed (created: {ok})");
        if ok {
            let _ = std::fs::remove_dir(&link);
        }
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn cancel_and_missing() {
        let root = tree("cancel");
        let e = folder_size(&root, &|| true, &mut |_| {}).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
        assert!(folder_size(Path::new(r"C:\definitely\not\here\rf"), &|| false, &mut |_| {}).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn wait(b: &SizeBatch) {
        let t = Instant::now();
        while !b.finished() {
            assert!(t.elapsed() < Duration::from_secs(10), "batch did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn batch_sizes_each_folder_and_fills_the_cache() {
        let (r1, r2) = (tree("batch1"), tree("batch2"));
        std::fs::write(r2.join("extra.bin"), vec![0u8; 24_000]).unwrap();
        let missing = PathBuf::from(r"C:\definitely\not\here\rf");
        let b = SizeBatch::start(vec![r1.clone(), r2.clone(), r1.join("empty"), missing]);
        wait(&b);
        let bytes = |i| match b.state(i) {
            SizeState::Done(s) => (s.bytes, s.files, s.dirs),
            s => panic!("{s:?}"),
        };
        assert_eq!(bytes(0), (76_000, 3, 3));
        assert_eq!(bytes(1), (100_000, 4, 3));
        assert_eq!(bytes(2), (0, 0, 0));
        assert_eq!(bytes(3), (0, 0, 0)); // unreadable: empty, but finished
        assert_eq!(shared_cache().get(&r2).map(|s| s.bytes), Some(100_000));
        // second time from the cache, same answer
        let b2 = SizeBatch::start(vec![r2.clone()]);
        wait(&b2);
        assert!(matches!(b2.state(0), SizeState::Done(s) if s.bytes == 100_000 && s.allocated.is_some()));
        shared_cache().invalidate(&r1);
        shared_cache().invalidate(&r2);
        std::fs::remove_dir_all(&r1).unwrap();
        std::fs::remove_dir_all(&r2).unwrap();
    }

    #[test]
    fn small_folder_is_not_held_back_by_a_big_one() {
        // a wide tree keeps the workers busy; a small folder started later still finishes first
        let big = std::env::temp_dir().join(format!("rf-fsize-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&big);
        for i in 0..400 {
            std::fs::create_dir_all(big.join(format!("d{i}/e/f"))).unwrap();
            std::fs::write(big.join(format!("d{i}/e/f/x")), b"1").unwrap();
        }
        let small = tree("small");
        let b1 = SizeBatch::start(vec![big.clone()]);
        let b2 = SizeBatch::start(vec![small.clone()]);
        wait(&b2);
        let big_done_first = b1.finished();
        wait(&b1);
        assert!(matches!(b2.state(0), SizeState::Done(s) if s.bytes == 76_000));
        assert!(matches!(b1.state(0), SizeState::Done(s) if s.bytes == 400 && s.dirs == 1200));
        assert!(!big_done_first, "the small folder waited for the big one");
        shared_cache().invalidate(&big);
        shared_cache().invalidate(&small);
        std::fs::remove_dir_all(&big).unwrap();
        std::fs::remove_dir_all(&small).unwrap();
    }

    #[test]
    fn cancelled_batch_finishes_without_caching() {
        let r = tree("batchcancel");
        let b = SizeBatch::start(vec![r.clone()]);
        b.cancel();
        wait(&b);
        // it may have finished before the cancel landed; if it was stopped nothing is cached
        if shared_cache().get(&r).is_none() {
            assert!(matches!(b.state(0), SizeState::Done(_)));
        }
        shared_cache().invalidate(&r);
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn cache_hits_and_invalidates() {
        let root = tree("cache");
        let cache = FolderSizeCache::default();
        assert!(cache.get(&root).is_none());
        let s1 = cache.get_or_compute(&root, &|| false, &mut |_| {}).unwrap();
        assert_eq!(s1.bytes, 76_000);
        assert_eq!(cache.get(&root), Some(s1));

        // Deep change: root mtime unchanged -> stale hit until invalidated.
        std::fs::write(root.join("a/b/z2.bin"), vec![0u8; 10]).unwrap();
        assert_eq!(cache.get(&root).unwrap().bytes, 76_000);
        cache.invalidate(&root);
        assert!(cache.get(&root).is_none());

        // Direct change: root mtime moves -> miss, recompute sees the new file.
        let s2 = cache.get_or_compute(&root, &|| false, &mut |_| {}).unwrap();
        assert_eq!(s2.bytes, 76_010);
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(root.join("new.txt"), b"hello").unwrap();
        assert!(cache.get(&root).is_none());
        assert_eq!(cache.get_or_compute(&root, &|| false, &mut |_| {}).unwrap().bytes, 76_015);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn fs(bytes: u64) -> FolderSize {
        FolderSize { bytes, files: 2, dirs: 1, allocated: Some(bytes + 100) }
    }

    fn temp_file(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rf-fsize-file-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("foldersizes.bin")
    }

    #[test]
    fn cache_file_round_trip() {
        let path = temp_file("rt");
        let a = FolderSizeCache::default();
        a.insert_at(Path::new(r"C:\Data\One"), fs(10), None);
        a.insert_at(Path::new(r"C:\Data\Two\"), FolderSize { allocated: None, ..fs(20) }, None);
        a.save_to(&path, 100).unwrap();
        assert!(!path.with_extension("tmp").exists(), "temporary file left behind");
        let b = FolderSizeCache::default();
        assert_eq!(b.load_from(&path), 2);
        // case-insensitive key, trailing slash ignored, "unknown on disk" stays unknown
        assert_eq!(b.provisional(Path::new(r"c:\data\one")), Some(fs(10)));
        assert_eq!(b.provisional(Path::new(r"C:\Data\Two")).map(|s| s.allocated), Some(None));
        // remembered values never count as measured
        assert!(b.get(Path::new(r"C:\Data\One")).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn bad_cache_files_are_ignored() {
        let path = temp_file("bad");
        let a = FolderSizeCache::default();
        a.insert_at(Path::new(r"C:\x"), fs(1), None);
        a.insert_at(Path::new(r"C:\y"), fs(2), None);
        a.save_to(&path, 100).unwrap();
        let good = std::fs::read(&path).unwrap();
        let load = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            FolderSizeCache::default().load_from(&path)
        };
        assert_eq!(load(&good), 2);
        assert_eq!(load(&good[..good.len() - 5]), 0, "truncated");
        assert_eq!(load(&good[..10]), 0, "cut in the header");
        let mut flipped = good.clone();
        flipped[20] ^= 0xFF;
        assert_eq!(load(&flipped), 0, "corrupt");
        assert_eq!(load(b"hello, this is not a cache file"), 0, "foreign");
        assert_eq!(load(&[]), 0, "empty");
        // a huge row count in a short file must not allocate or crash
        let mut lie = good[..12].to_vec();
        lie[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let sum = fnv1a(&lie);
        lie.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(load(&lie), 0, "lying count");
        assert_eq!(FolderSizeCache::default().load_from(&path.with_file_name("missing.bin")), 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cache_file_keeps_the_most_recent() {
        let path = temp_file("bound");
        let a = FolderSizeCache::default();
        for i in 0..50u64 {
            a.insert_at(Path::new(&format!(r"C:\f{i}")), fs(i), None);
        }
        a.save_to(&path, 20).unwrap();
        let b = FolderSizeCache::default();
        assert_eq!(b.load_from(&path), 20);
        assert!(b.provisional(Path::new(r"C:\f29")).is_none());
        assert!(b.provisional(Path::new(r"C:\f30")).is_some());
        assert!(b.provisional(Path::new(r"C:\f49")).is_some());
        // a reader with less room takes the newest of what is stored
        let small = FolderSizeCache::new(16);
        assert_eq!(small.load_from(&path), 16);
        assert!(small.provisional(Path::new(r"C:\f49")).is_some() && small.provisional(Path::new(r"C:\f33")).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A remembered size is shown at once, then replaced by the fresh measurement; a change deep
    /// inside (which leaves the folder's own date alone) shows up.
    #[test]
    fn provisional_value_is_replaced_by_a_fresh_measurement() {
        let root = tree("prov");
        let path = temp_file("prov");
        let a = FolderSizeCache::default();
        let s1 = a.get_or_compute(&root, &|| false, &mut |_| {}).unwrap();
        assert_eq!(s1.bytes, 76_000);
        a.save_to(&path, 100).unwrap();
        // something deep changes while the app is closed
        std::fs::write(root.join("a/b/z2.bin"), vec![0u8; 500]).unwrap();

        let fresh = shared_cache();
        fresh.invalidate(&root);
        assert!(fresh.load_from(&path) >= 1);
        assert_eq!(fresh.provisional(&root).map(|s| s.bytes), Some(76_000));
        assert!(fresh.get(&root).is_none(), "a remembered value must not pass as measured");

        let b = SizeBatch::start(vec![root.clone()]);
        // right after the start the old value is what the batch shows (or the fresh one, if the
        // workers were quicker): never a partial count
        assert!(matches!(b.state(0), SizeState::Provisional(s) if s.bytes == 76_000) || matches!(b.state(0), SizeState::Done(_)));
        wait(&b);
        assert!(matches!(b.state(0), SizeState::Done(s) if s.bytes == 76_500));
        assert!(fresh.provisional(&root).is_none());
        assert_eq!(fresh.get(&root).map(|s| s.bytes), Some(76_500));
        fresh.invalidate(&root);
        std::fs::remove_dir_all(&root).unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn changes_are_written_in_the_background() {
        let path = temp_file("bg");
        let cache: &'static FolderSizeCache = Box::leak(Box::default());
        cache.persist_with(path.clone(), Duration::from_millis(50), 100);
        for i in 0..5u64 {
            cache.insert_at(Path::new(&format!(r"C:\bg{i}")), fs(i), None);
        }
        let t = Instant::now();
        while !path.exists() {
            assert!(t.elapsed() < Duration::from_secs(5), "cache file was not written");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(FolderSizeCache::default().load_from(&path), 5);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
