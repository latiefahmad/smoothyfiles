//! Our own copy / move engine: many files at once with `CopyFile2` (attributes, times and
//! alternate data streams come along), instead of the shell engine's one-file-at-a-time
//! pipeline. Names that already exist at the destination are ours to handle too (like Files):
//! folders merge, and every clashing file is put to the user once, in one list (replace, skip
//! or keep both).
//!
//! * Moves on one volume are renames (instant); onto another volume a copy, after which each
//!   source file is deleted only once its copy checks out.
//! * "Replace" copies to a temporary name first and swaps it in at the end, so a failed or
//!   cancelled copy never costs the file that was there.
//! * Small files are spread over folders: threads writing into the same folder wait on its
//!   lock (20 000 files: ~3 400/s in folder order, ~6 500/s spread).
//! * Pause, a speed limit and byte-accurate progress run in CopyFile2's progress callback.
//! * Crash-safe: the planned job list goes to a journal (synced to disk) before the first byte
//!   is copied, and finished files are recorded as they complete. If the app dies mid-copy,
//!   [`interrupted`] finds the job on the next start and [`resume`] carries it on with the same
//!   decisions (conflicts aren't asked again); every file the journal or the destination says
//!   is done is re-checked against its source before it's skipped.
//!
//! Left to the shell engine, so behavior stays Explorer's where it matters: items containing
//! links / junctions, a folder into itself, paths that aren't plain file system paths, and
//! files that fail here (access denied, in use, …) — retried through the shell at the end,
//! which can ask for elevation.
//!
//! Blocking: call from a worker thread.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, COPY_FILE_RESTARTABLE, COPYFILE2_CALLBACK_CHUNK_FINISHED, COPYFILE2_EXTENDED_PARAMETERS, COPYFILE2_MESSAGE,
    COPYFILE2_MESSAGE_ACTION, COPYFILE2_PROGRESS_CANCEL, COPYFILE2_PROGRESS_CONTINUE, CopyFile2, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MoveFileExW, SetFileAttributesW,
};
use windows::core::PCWSTR;

use crate::fileops::{Tracker, Transfer};
use crate::wide::long_path;

/// Files above this size are copied unbuffered (no point filling the cache with them).
/// Files above this size are started first (they set the pace; small ones fill the gaps).
const BIG: u64 = 8 << 20;
/// With a journal, files above this size are copied restartably (Windows keeps the progress in
/// the target), so a copy cut off by a crash carries on mid-file instead of starting over. The
/// bookkeeping costs about 30 % (1 GB: 364 ms against 285 ms), and a file under 2 GB copies
/// again in about a second, so only the files where carrying on saves real time use it.
const RESTARTABLE_MIN: u64 = 2 << 30;
/// A rename counts like this many bytes of progress.
const RENAME_WEIGHT: u64 = 64 << 10;

/// How copies are checked after writing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Verify {
    #[default]
    Off,
    /// Same size and modification time.
    Quick,
    /// Same bytes, read back from the disk (not from the cache).
    Full,
}

/// A name that already exists at the destination.
#[derive(Clone, Debug)]
pub struct Conflict {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub src_dir: bool,
    pub dst_dir: bool,
    pub src_size: u64,
    pub dst_size: u64,
    pub src_time: Option<SystemTime>,
    pub dst_time: Option<SystemTime>,
}

impl Conflict {
    /// A file against a folder (or the other way round): it can't replace, only skip or keep both.
    pub fn mismatched(&self) -> bool {
        self.src_dir != self.dst_dir
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolve {
    Replace,
    Skip,
    KeepBoth,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Copy,
    Rename,
}

/// One file to copy, or one item to rename into place.
#[derive(Clone, Debug)]
struct Job {
    src: PathBuf,
    dst: PathBuf,
    size: u64,
    op: Op,
    /// The destination exists and is to be replaced.
    replace: bool,
    /// A restartable copy cut off earlier: carry it on where it stopped.
    resume: bool,
}

/// What a transfer will do, worked out before anything is written.
#[derive(Default)]
struct Plan {
    /// Folders to create (copies), parents first.
    dirs: Vec<PathBuf>,
    jobs: Vec<Job>,
    conflicts: Vec<(Conflict, Job)>,
    /// (source, destination) of every item this transfer creates, for undo.
    undo: Vec<(PathBuf, PathBuf)>,
    /// (source folder, its new copy): attributes are copied once the contents are in.
    new_dirs: Vec<(PathBuf, PathBuf)>,
    /// Source folders whose contents are taken file by file (moves remove them when empty).
    src_dirs: Vec<PathBuf>,
    /// Top-level sources for the shell engine.
    whole: Vec<PathBuf>,
    /// Destination paths this plan writes (lowercase), so "keep both" picks a free name.
    taken: HashSet<String>,
}

/// Whether two paths are the same file or folder (volume serial and file index).
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    // (std opens long paths and resolves `..`; no access rights needed, folders too)
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let id = |p: &Path| -> Option<(u32, u32, u32)> {
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(7)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
            .ok()?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(f.as_raw_handle()), &mut info) }.ok()?;
        Some((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
    };
    matches!((id(a), id(b)), (Some(x), Some(y)) if x == y)
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

fn is_reparse(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// A link or junction somewhere in the tree: the shell copes with those.
struct HasLink;

impl Plan {
    /// Plans one top-level source; on a link inside it, nothing of it is kept and it goes to
    /// the shell whole.
    fn top(&mut self, src: &Path, dest_dir: &Path, rename: bool) {
        let Some(name) = src.file_name() else {
            self.whole.push(src.to_path_buf());
            return;
        };
        let dst = dest_dir.join(name);
        let meta = match std::fs::symlink_metadata(src) {
            Ok(m) if !is_reparse(&m) => m,
            _ => {
                self.whole.push(src.to_path_buf());
                return;
            }
        };
        // a folder into itself or below itself: the shell refuses it with its own message
        if meta.is_dir() && inside(dest_dir, src) {
            self.whole.push(src.to_path_buf());
            return;
        }
        let mut sub = Plan { taken: std::mem::take(&mut self.taken), ..Default::default() };
        match sub.visit(src, &dst, &meta, rename) {
            Ok(()) => {
                self.taken = std::mem::take(&mut sub.taken);
                self.dirs.extend(sub.dirs);
                self.jobs.extend(sub.jobs);
                self.conflicts.extend(sub.conflicts);
                self.undo.extend(sub.undo);
                self.new_dirs.extend(sub.new_dirs);
                self.src_dirs.extend(sub.src_dirs);
            }
            Err(HasLink) => {
                // drop what it reserved
                let mut taken = std::mem::take(&mut sub.taken);
                for j in sub.jobs.iter().chain(sub.conflicts.iter().map(|c| &c.1)) {
                    taken.remove(&key(&j.dst));
                }
                for d in &sub.dirs {
                    taken.remove(&key(d));
                }
                self.taken = taken;
                self.whole.push(src.to_path_buf());
            }
        }
    }

    fn visit(&mut self, src: &Path, dst: &Path, meta: &std::fs::Metadata, rename: bool) -> Result<(), HasLink> {
        // already planned by another source of this transfer (same name from two folders)
        if self.taken.contains(&key(dst)) {
            let free = self.free_name(dst, meta.is_dir());
            return self.visit(src, &free, meta, rename);
        }
        let there = std::fs::symlink_metadata(dst).ok();
        // the destination IS the source, reached by another path (Z: and \\nas\share, a
        // junction, subst): nothing to do. As a conflict, "Replace" wrote the file over itself
        // and a move then deleted the "source", the only copy.
        if there.is_some() && same_file(src, dst) {
            return Ok(());
        }
        match there {
            None if rename => {
                self.taken.insert(key(dst));
                self.jobs.push(Job { src: src.into(), dst: dst.into(), size: RENAME_WEIGHT, op: Op::Rename, replace: false, resume: false });
                self.undo.push((src.into(), dst.into()));
            }
            None if meta.is_dir() => {
                self.new_tree(src, dst)?;
                self.undo.push((src.into(), dst.into()));
            }
            None => {
                self.taken.insert(key(dst));
                self.jobs.push(Job { src: src.into(), dst: dst.into(), size: meta.len(), op: Op::Copy, replace: false, resume: false });
                self.undo.push((src.into(), dst.into()));
            }
            // folder onto folder: merge, item by item
            Some(d) if meta.is_dir() && d.is_dir() && !is_reparse(&d) => {
                self.src_dirs.push(src.into());
                let rd = std::fs::read_dir(src).map_err(|_| HasLink)?;
                for e in rd.flatten() {
                    let m = std::fs::symlink_metadata(e.path()).map_err(|_| HasLink)?;
                    if is_reparse(&m) {
                        return Err(HasLink);
                    }
                    self.visit(&e.path(), &dst.join(e.file_name()), &m, rename)?;
                }
            }
            Some(d) => {
                use std::os::windows::fs::MetadataExt;
                let c = Conflict {
                    src: src.into(),
                    dst: dst.into(),
                    src_dir: meta.is_dir(),
                    dst_dir: d.is_dir(),
                    src_size: if meta.is_dir() { 0 } else { meta.len() },
                    dst_size: if d.is_dir() { 0 } else { d.file_size() },
                    src_time: meta.modified().ok(),
                    dst_time: d.modified().ok(),
                };
                if meta.is_dir() {
                    // walked now, so a link inside is found before anything is asked
                    check_tree(src)?;
                }
                let size = if rename { RENAME_WEIGHT } else { meta.len() };
                self.conflicts.push((c, Job { src: src.into(), dst: dst.into(), size, op: if rename { Op::Rename } else { Op::Copy }, replace: false, resume: false }));
            }
        }
        Ok(())
    }

    /// A whole new folder: its folders and files (copies).
    fn new_tree(&mut self, src: &Path, dst: &Path) -> Result<(), HasLink> {
        self.taken.insert(key(dst));
        self.dirs.push(dst.into());
        self.new_dirs.push((src.into(), dst.into()));
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
        while let Some((from, to)) = stack.pop() {
            let rd = std::fs::read_dir(&from).map_err(|_| HasLink)?;
            for e in rd.flatten() {
                let m = e.metadata().map_err(|_| HasLink)?;
                if is_reparse(&m) {
                    return Err(HasLink);
                }
                let (a, b) = (e.path(), to.join(e.file_name()));
                if m.is_dir() {
                    self.dirs.push(b.clone());
                    self.new_dirs.push((a.clone(), b.clone()));
                    stack.push((a, b));
                } else {
                    self.jobs.push(Job { src: a, dst: b, size: m.len(), op: Op::Copy, replace: false, resume: false });
                }
            }
        }
        Ok(())
    }

    /// Applies the user's answers to the conflicts.
    fn resolve(&mut self, answers: &[Resolve]) {
        let conflicts = std::mem::take(&mut self.conflicts);
        for ((c, mut job), r) in conflicts.into_iter().zip(answers.iter().copied()) {
            let r = if r == Resolve::Replace && c.mismatched() { Resolve::KeepBoth } else { r };
            match r {
                Resolve::Skip => {}
                Resolve::Replace => {
                    job.replace = true;
                    self.undo.push((job.src.clone(), job.dst.clone()));
                    self.jobs.push(job);
                }
                Resolve::KeepBoth => {
                    let dst = self.free_name(&c.dst, c.src_dir);
                    if job.op == Op::Rename || !c.src_dir {
                        self.taken.insert(key(&dst));
                        self.undo.push((job.src.clone(), dst.clone()));
                        self.jobs.push(Job { dst, ..job });
                    } else if self.new_tree(&c.src, &dst).is_ok() {
                        self.undo.push((c.src.clone(), dst));
                    }
                }
            }
        }
    }

    /// "name (2).ext", "name (3).ext", … : the first that neither exists nor is planned.
    fn free_name(&self, dst: &Path, is_dir: bool) -> PathBuf {
        let dir = dst.parent().unwrap_or(Path::new(""));
        let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 && !is_dir => (name[..i].to_string(), name[i..].to_string()),
            _ => (name.clone(), String::new()),
        };
        (2..)
            .map(|n| dir.join(format!("{stem} ({n}){ext}")))
            .find(|p| !self.taken.contains(&key(p)) && std::fs::symlink_metadata(p).is_err())
            .expect("a free name")
    }
}

/// Errors if there is a link or junction anywhere under `dir`.
fn check_tree(dir: &Path) -> Result<(), HasLink> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|_| HasLink)?.flatten() {
            let m = e.metadata().map_err(|_| HasLink)?;
            if is_reparse(&m) {
                return Err(HasLink);
            }
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

/// Plans `sources` into `dest_dir`: what goes where, and what clashes.
fn plan(sources: &[PathBuf], dest_dir: &Path, rename: bool) -> Plan {
    let mut p = Plan::default();
    for s in sources {
        p.top(s, dest_dir, rename);
    }
    p
}

/// Big files first, then the rest taken from each folder in turn.
fn order(jobs: Vec<Job>) -> Vec<Job> {
    let (mut big, small): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| j.size >= BIG);
    big.sort_by_key(|j| std::cmp::Reverse(j.size));
    let mut folders: Vec<std::collections::VecDeque<Job>> = Vec::new();
    let mut at: std::collections::HashMap<PathBuf, usize> = Default::default();
    for j in small {
        let parent = j.dst.parent().map(Path::to_path_buf).unwrap_or_default();
        let ix = *at.entry(parent).or_insert_with(|| {
            folders.push(Default::default());
            folders.len() - 1
        });
        folders[ix].push_back(j);
    }
    let mut out = big;
    loop {
        let before = out.len();
        for q in folders.iter_mut() {
            if let Some(j) = q.pop_front() {
                out.push(j);
            }
        }
        if out.len() == before {
            break;
        }
    }
    out
}

/// Progress, pause, speed limit and cancel, shared by the copy threads.
struct Run<'a> {
    tracker: &'a Tracker,
    /// In temporary names (".name.<tag>.rfpart"): the journal's id (the same when the job is
    /// resumed, so a restartable copy finds its partial file again), else the process id.
    tag: String,
    /// Journaled: big files are copied restartably.
    restartable: bool,
    total: u64,
    done: AtomicU64,
    /// Turned non-zero to stop CopyFile2 (it removes the partial file).
    cancel: AtomicI32,
    /// Speed limit clock: (since, bytes counted since then).
    clock: Mutex<(Instant, u64)>,
    last_report: Mutex<Instant>,
}

impl Run<'_> {
    fn cancelled(&self) -> bool {
        if self.tracker.cancel.load(Ordering::Relaxed) {
            self.cancel.store(1, Ordering::Relaxed);
        }
        self.cancel.load(Ordering::Relaxed) != 0
    }

    /// Blocks while paused; the paused time doesn't count against the speed limit.
    fn wait_if_paused(&self) {
        if !self.tracker.paused.load(Ordering::Relaxed) {
            return;
        }
        let from = Instant::now();
        while self.tracker.paused.load(Ordering::Relaxed) && !self.cancelled() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut c = self.clock.lock().unwrap();
        c.0 += from.elapsed();
    }

    fn add(&self, bytes: u64, name: Option<&Path>) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
        let limit = self.tracker.limit.load(Ordering::Relaxed);
        if limit > 0 {
            let wait = {
                let mut c = self.clock.lock().unwrap();
                c.1 += bytes;
                let due = Duration::from_secs_f64(c.1 as f64 / limit as f64);
                due.saturating_sub(c.0.elapsed())
            };
            if !wait.is_zero() {
                std::thread::sleep(wait.min(Duration::from_millis(500)));
            }
        }
        let mut last = self.last_report.lock().unwrap();
        if last.elapsed() >= Duration::from_millis(50) {
            *last = Instant::now();
            drop(last);
            self.report(name);
        }
    }

    fn report(&self, name: Option<&Path>) {
        let d = self.done.load(Ordering::Relaxed).min(self.total);
        let permille = (d as u128 * 1000 / self.total.max(1) as u128) as u32;
        (self.tracker.on_progress)(1000, permille, name.and_then(|n| n.file_name()).map(|n| n.to_string_lossy().into_owned()));
    }
}

/// CopyFile2's per-file callback context.
struct FileCtx<'a> {
    run: &'a Run<'a>,
    name: &'a Path,
    sent: Cell<u64>,
}

unsafe extern "system" fn on_chunk(msg: *const COPYFILE2_MESSAGE, ctx: *const core::ffi::c_void) -> COPYFILE2_MESSAGE_ACTION {
    // SAFETY: CopyFile2 calls back on the copying thread while `ctx` (a FileCtx on that
    // thread's stack) is alive.
    let (msg, ctx) = unsafe { (&*msg, &*(ctx as *const FileCtx)) };
    if msg.Type == COPYFILE2_CALLBACK_CHUNK_FINISHED {
        let total = unsafe { msg.Info.ChunkFinished.uliTotalBytesTransferred };
        let delta = total.saturating_sub(ctx.sent.replace(total));
        ctx.run.add(delta, Some(ctx.name));
    }
    ctx.run.wait_if_paused();
    if ctx.run.cancelled() { COPYFILE2_PROGRESS_CANCEL } else { COPYFILE2_PROGRESS_CONTINUE }
}

/// `restart`: carry on a restartable copy into an existing partial `dst`.
fn copy_file(src: &Path, dst: &Path, size: u64, run: &Run, restart: bool) -> windows::core::Result<()> {
    let (s, d) = (long_path(src), long_path(dst));
    // Buffered, like Explorer: unbuffered writes of big files measured twice as slow
    // (1 GB: 755 ms against Windows' 348 ms; buffered 285 against 265).
    let mut flags = if restart { 0 } else { COPY_FILE_FAIL_IF_EXISTS.0 };
    if run.restartable && size >= RESTARTABLE_MIN {
        flags |= COPY_FILE_RESTARTABLE.0;
    }
    let ctx = FileCtx { run, name: src, sent: Cell::new(0) };
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: windows::Win32::Storage::FileSystem::COPYFILE_FLAGS(flags),
        pfCancel: run.cancel.as_ptr() as *mut windows_core::BOOL,
        pProgressRoutine: Some(on_chunk),
        pvCallbackContext: &ctx as *const FileCtx as *mut core::ffi::c_void,
    };
    let r = unsafe { CopyFile2(PCWSTR(s.as_ptr()), PCWSTR(d.as_ptr()), Some(&params)) };
    // whatever the callback didn't count (an empty file reports no chunk)
    run.add(size.max(1).saturating_sub(ctx.sent.get()), None);
    r
}

/// Moves `from` over `to` (replacing it when asked), clearing a read-only flag in the way.
fn move_into_place(from: &Path, to: &Path, replace: bool) -> windows::core::Result<()> {
    let (f, t) = (long_path(from), long_path(to));
    let flags = if replace { MOVEFILE_REPLACE_EXISTING } else { MOVE_FILE_FLAGS(0) };
    let r = unsafe { MoveFileExW(PCWSTR(f.as_ptr()), PCWSTR(t.as_ptr()), flags) };
    if r.is_err() && replace {
        unsafe {
            let a = GetFileAttributesW(PCWSTR(t.as_ptr()));
            if a != u32::MAX && a & FILE_ATTRIBUTE_READONLY.0 != 0 {
                let _ = SetFileAttributesW(PCWSTR(t.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(a & !FILE_ATTRIBUTE_READONLY.0));
                return MoveFileExW(PCWSTR(f.as_ptr()), PCWSTR(t.as_ptr()), flags);
            }
        }
    }
    r
}

/// A temporary name next to `dst` for a copy that replaces it.
fn part_name(dst: &Path, tag: &str) -> PathBuf {
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dst.with_file_name(format!(".{name}.{tag}.rfpart"))
}

/// A copy into `to`; a resumed one carries on, and starts over if Windows can't (the partial
/// file is removed first).
fn copy_or_resume(j: &Job, to: &Path, run: &Run) -> windows::core::Result<()> {
    if j.resume && std::fs::symlink_metadata(to).is_ok() {
        match copy_file(&j.src, to, j.size, run, true) {
            Ok(()) => {
                log::info!("fastcopy: carried on {} where it stopped", j.dst.display());
                return Ok(());
            }
            Err(e) if run.cancelled() => return Err(e),
            Err(e) => {
                log::info!("fastcopy: resume of {} failed ({e}), copying it again", j.dst.display());
                let _ = std::fs::remove_file(to);
            }
        }
    }
    copy_file(&j.src, to, j.size, run, false)
}

/// One job; `Err` = leave it to the shell.
fn run_job(j: &Job, run: &Run) -> windows::core::Result<()> {
    match j.op {
        Op::Rename => {
            let r = move_into_place(&j.src, &j.dst, j.replace);
            run.add(j.size, Some(&j.src));
            r
        }
        Op::Copy if j.replace => {
            let tmp = part_name(&j.dst, &run.tag);
            if !j.resume {
                let _ = std::fs::remove_file(&tmp);
            }
            copy_or_resume(j, &tmp, run)?;
            move_into_place(&tmp, &j.dst, true).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        }
        Op::Copy => copy_or_resume(j, &j.dst, run),
    }
}

/// What a transfer did here: undo pairs, files copied (source, copy), and what is left for
/// the shell.
#[derive(Default)]
pub struct Outcome {
    pub undo: Vec<(PathBuf, PathBuf)>,
    /// Single files that failed here: (source, destination folder).
    pub failed: Vec<(PathBuf, PathBuf)>,
    /// Top-level sources the shell should transfer whole.
    pub whole: Vec<PathBuf>,
    copied: Vec<(PathBuf, PathBuf)>,
    src_dirs: Vec<PathBuf>,
    cancelled: bool,
}

/// Plans, asks about conflicts, journals the plan and runs a transfer with our engine (`rename`
/// = a move on one volume). Everything it can't do is in the returned leftovers. The journal is
/// returned open: the caller closes it once `finish` is done (a crash before that leaves it).
fn execute(kind: Transfer, sources: &[PathBuf], dest_dir: &Path, tracker: &Tracker, threads: usize, rename: bool) -> (Outcome, Option<Jr>) {
    let mut p = plan(sources, dest_dir, rename);
    if !p.conflicts.is_empty() {
        let list: Vec<Conflict> = p.conflicts.iter().map(|c| c.0.clone()).collect();
        let answers = match &tracker.ask {
            Some(ask) => ask(&list),
            None => Some(vec![Resolve::KeepBoth; list.len()]),
        };
        match answers {
            Some(a) => p.resolve(&a),
            None => return (Outcome { cancelled: true, ..Default::default() }, None),
        }
    }
    p.jobs = order(std::mem::take(&mut p.jobs));
    let jr = journal_start(kind, rename, tracker.verify_mode(), sources, dest_dir, &p);
    let n = p.jobs.len();
    let out = run_jobs(p, tracker, threads, jr.as_ref().map(|j| (j, (0..n).collect::<Vec<_>>())), Vec::new());
    (out, jr)
}

/// Creates the plan's folders and runs its jobs (already ordered) on `threads` threads.
/// `journal`: where finished jobs are recorded, with each job's index in the journal.
/// `already`: (source, copy) pairs done before (a resumed job), checked and finished like the
/// rest.
fn run_jobs(mut p: Plan, tracker: &Tracker, threads: usize, journal: Option<(&Jr, Vec<usize>)>, already: Vec<(PathBuf, PathBuf)>) -> Outcome {
    for d in &p.dirs {
        if let Err(e) = std::fs::create_dir(d) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                log::warn!("fastcopy: mkdir {}: {e}", d.display());
            }
        }
    }
    let jobs = std::mem::take(&mut p.jobs);
    let tag = journal.as_ref().and_then(|(jr, _)| jr.path.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| std::process::id().to_string());
    let run = Run {
        tracker,
        tag,
        restartable: journal.is_some(),
        total: jobs.iter().map(|j| j.size.max(1)).sum::<u64>().max(1),
        done: AtomicU64::new(0),
        cancel: AtomicI32::new(0),
        clock: Mutex::new((Instant::now(), 0)),
        last_report: Mutex::new(Instant::now() - Duration::from_secs(1)),
    };
    let next = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    let done = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, jobs.len().max(1)) {
            s.spawn(|| {
                loop {
                    run.wait_if_paused();
                    if run.cancelled() {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(j) = jobs.get(i) else { return };
                    match run_job(j, &run) {
                        Ok(()) => {
                            done.lock().unwrap().push(i);
                            if let Some((jr, ix)) = &journal {
                                jr.done(ix[i]);
                            }
                        }
                        Err(e) => {
                            if !run.cancelled() {
                                log::info!("fastcopy: {} → shell ({e})", j.src.display());
                                failed.lock().unwrap().push(i);
                            }
                        }
                    }
                }
            });
        }
    });
    run.report(None);
    if let Some((jr, _)) = &journal {
        jr.sync();
    }
    let cancelled = run.cancelled();
    // folder attributes (hidden, read-only, …) once their contents are in
    for (src, dst) in &p.new_dirs {
        if let Ok(m) = std::fs::metadata(src) {
            use std::os::windows::fs::MetadataExt;
            let w = long_path(dst);
            unsafe {
                let _ = SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(m.file_attributes()));
            }
        }
    }
    let failed: Vec<(PathBuf, PathBuf)> = failed
        .into_inner()
        .unwrap()
        .into_iter()
        .filter_map(|i| jobs[i].dst.parent().map(|d| (jobs[i].src.clone(), d.to_path_buf())))
        .collect();
    let mut copied = already;
    copied.extend(done.into_inner().unwrap().into_iter().filter(|&i| jobs[i].op == Op::Copy).map(|i| (jobs[i].src.clone(), jobs[i].dst.clone())));
    Outcome { undo: p.undo, failed, whole: p.whole, copied, src_dirs: p.src_dirs, cancelled }
}

/// Copies `sources` into `dest_dir` in parallel (conflicts as `tracker.ask` says; without it
/// both are kept). Returns the undo pairs and the leftovers for the shell.
pub fn copy(sources: &[PathBuf], dest_dir: &Path, tracker: &Tracker, threads: usize) -> Outcome {
    let (out, jr) = execute(Transfer::Copy, sources, dest_dir, tracker, threads, false);
    if let Some(j) = jr {
        j.close();
    }
    out
}

/// Copy or move with our engine where it can, the shell engine for the rest. Returns like
/// `fileops::transfer_tracked`; undo pairs land in `tracker.results`. A verification failure
/// is an `Err` (the copies stay).
pub fn transfer(kind: Transfer, sources: &[PathBuf], dest_dir: &Path, tracker: Arc<Tracker>) -> anyhow::Result<bool> {
    // pasting where the items already are ("x - Copy"), and anything not on a plain file
    // system path: the shell's way
    let same_dir = sources.iter().all(|s| s.parent() == Some(dest_dir));
    if same_dir || !local_fs(dest_dir) || !sources.iter().all(|s| local_fs(s)) {
        return crate::fileops::transfer_tracked(kind, sources, dest_dir, tracker);
    }
    let (here, there): (Vec<PathBuf>, Vec<PathBuf>) = sources.iter().cloned().partition(|s| kind == Transfer::Move && same_volume(s, dest_dir));
    let mut ok = true;
    if !here.is_empty() {
        let (out, jr) = execute(Transfer::Move, &here, dest_dir, &tracker, 4, true);
        let r = finish(Transfer::Move, &here, dest_dir, &tracker, out);
        if let Some(j) = jr {
            j.close();
        }
        ok &= r?;
    }
    if !there.is_empty() && !tracker.cancel.load(Ordering::Relaxed) {
        let threads = threads_for(&there, dest_dir);
        let (out, jr) = execute(kind, &there, dest_dir, &tracker, threads, false);
        let r = finish(kind, &there, dest_dir, &tracker, out);
        if let Some(j) = jr {
            j.close();
        }
        ok &= r?;
    }
    for s in sources {
        if let Some(p) = s.parent() {
            crate::shell::notify_updated(p);
        }
    }
    crate::shell::notify_updated(dest_dir);
    Ok(ok && !tracker.cancel.load(Ordering::Relaxed))
}

/// After our engine: the shell for the leftovers, verification, and for moves the removal of
/// what was moved.
fn finish(kind: Transfer, sources: &[PathBuf], dest_dir: &Path, tracker: &Arc<Tracker>, out: Outcome) -> anyhow::Result<bool> {
    tracker.results.lock().unwrap().extend(out.undo.iter().cloned());
    if out.cancelled || tracker.cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let mut ok = true;
    if !out.whole.is_empty() {
        ok &= crate::fileops::transfer_tracked(kind, &out.whole, dest_dir, tracker.clone())?;
    }
    // failed single files: one shell call per destination folder
    let mut by_dir: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for (f, d) in &out.failed {
        match by_dir.iter_mut().find(|(x, _)| x == d) {
            Some((_, v)) => v.push(f.clone()),
            None => by_dir.push((d.clone(), vec![f.clone()])),
        }
    }
    for (d, fs) in by_dir {
        // the shell reports its own results; those files' undo is covered by their folder
        let t = Arc::new(Tracker::new(Box::new(|_, _, _| {}), tracker.cancel.clone()));
        ok &= crate::fileops::transfer_tracked(kind, &fs, &d, t)?;
    }
    // checks: asked for, or a move about to delete its sources
    let mode = match (tracker.verify_mode(), kind) {
        (Verify::Off, Transfer::Move) => Verify::Quick,
        (m, _) => m,
    };
    let bad = check(&out.copied, mode, tracker);
    if tracker.cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }
    if kind == Transfer::Move {
        // only sources whose copy checked out go; skipped and failed ones stay
        let bad_set: HashSet<&PathBuf> = bad.iter().collect();
        let files: Vec<PathBuf> = out.copied.iter().filter(|(s, _)| !bad_set.contains(s)).map(|(s, _)| s.clone()).collect();
        // folders emptied by the move: merged ones, and whole ones copied across volumes
        let mut dirs: Vec<PathBuf> = out.src_dirs.clone();
        for (s, _) in &out.undo {
            if s.is_dir() && sources.iter().any(|t| s.starts_with(t)) {
                walk_dirs(s, &mut dirs);
            }
        }
        dirs.sort_by_key(|d| d.components().count());
        dirs.dedup();
        if !crate::fastdelete::remove_verified(&files, &dirs) {
            log::info!("fastcopy: some sources stay (skipped, failed or changed meanwhile)");
        }
    }
    if !bad.is_empty() {
        let names: Vec<String> = bad.iter().take(5).filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        anyhow::bail!("{}: {} ({})", NOT_VERIFIED, bad.len(), names.join(", "));
    }
    Ok(ok)
}

// ---------------------------------------------------------------- crash-safe job journal
//
// One file per running plan in the journal folder, records (see `journal`):
//   B kind(c|m) rename(0|1) verify(0|1|2) dest sources…   the job
//   D dir · N src dst · S src_dir · W whole                 the plan's folders and shell items
//   J op(c|r) replace(0|1) size src dst                    one per job, in run order
//   P                                                       the plan is complete (synced)
//   F index                                                 job `index` finished (batched)
// A journal without P never started copying. Closing a journal deletes it, so what's left in the
// folder is exactly the jobs that were cut off.

static JOURNAL_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Finished-file records are flushed to disk at most this often (a lost one only means that
/// file is checked again on resume).
const JOURNAL_SYNC: Duration = Duration::from_millis(250);

/// Where journals live; set once at startup. Without it nothing is journaled.
pub fn set_journal_dir(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = JOURNAL_DIR.set(dir);
}

struct Jr {
    w: Mutex<(crate::journal::Writer, Instant)>,
    path: PathBuf,
}

impl Jr {
    fn done(&self, index: usize) {
        let mut g = self.w.lock().unwrap();
        let _ = g.0.record(&["F", &index.to_string()]);
        if g.1.elapsed() >= JOURNAL_SYNC {
            let _ = g.0.sync();
            g.1 = Instant::now();
        }
    }
    fn sync(&self) {
        let _ = self.w.lock().unwrap().0.sync();
    }
    /// The job ended (done, failed or cancelled by the user): nothing to resume.
    fn close(self) {
        let Jr { w, path } = self;
        drop(w);
        let _ = std::fs::remove_file(&path);
    }
}

fn ps(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn journal_start(kind: Transfer, rename: bool, verify: Verify, sources: &[PathBuf], dest: &Path, p: &Plan) -> Option<Jr> {
    let dir = JOURNAL_DIR.get()?;
    static N: AtomicUsize = AtomicUsize::new(0);
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let path = dir.join(format!("{}-{stamp}-{}.jrn", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut w = crate::journal::Writer::create(&path).map_err(|e| log::warn!("journal: {}: {e}", path.display())).ok()?;
    let b = |x: bool| if x { "1" } else { "0" };
    let written = (|| -> std::io::Result<()> {
        let mut head = vec!["B".to_string(), (if kind == Transfer::Move { "m" } else { "c" }).into(), b(rename).into(), (verify as u8).to_string(), ps(dest)];
        head.extend(sources.iter().map(|x| ps(x)));
        w.record(&head.iter().map(String::as_str).collect::<Vec<_>>())?;
        for d in &p.dirs {
            w.record(&["D", &ps(d)])?;
        }
        for (a, c) in &p.new_dirs {
            w.record(&["N", &ps(a), &ps(c)])?;
        }
        for d in &p.src_dirs {
            w.record(&["S", &ps(d)])?;
        }
        for x in &p.whole {
            w.record(&["W", &ps(x)])?;
        }
        for j in &p.jobs {
            w.record(&["J", if j.op == Op::Copy { "c" } else { "r" }, b(j.replace), &j.size.to_string(), &ps(&j.src), &ps(&j.dst)])?;
        }
        w.record(&["P"])?;
        w.sync()
    })();
    if let Err(e) = written {
        log::warn!("journal: {}: {e}", path.display());
        drop(w);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(Jr { w: Mutex::new((w, Instant::now())), path })
}

/// A job that was cut off (the app closed or crashed while it ran).
#[derive(Clone, Debug)]
pub struct Interrupted {
    pub journal: PathBuf,
    pub kind: Transfer,
    pub verify: Verify,
    pub sources: Vec<PathBuf>,
    pub dest: PathBuf,
    /// Jobs in the plan, and how many were recorded as finished.
    pub files: usize,
    pub done: usize,
}

struct Loaded {
    info: Interrupted,
    rename: bool,
    records: Vec<Vec<String>>,
}

fn load(path: &Path) -> Option<Loaded> {
    let records = crate::journal::read(path).ok()?;
    let head = records.first().filter(|r| r.len() >= 5 && r[0] == "B")?;
    if !records.iter().any(|r| r[0] == "P") {
        return None;
    }
    let kind = if head[1] == "m" { Transfer::Move } else { Transfer::Copy };
    let verify = match head[3].as_str() {
        "1" => Verify::Quick,
        "2" => Verify::Full,
        _ => Verify::Off,
    };
    let files = records.iter().filter(|r| r[0] == "J").count();
    let done = records.iter().filter(|r| r[0] == "F").filter_map(|r| r.get(1)?.parse::<usize>().ok()).collect::<HashSet<_>>().len();
    Some(Loaded {
        rename: head[2] == "1",
        info: Interrupted { journal: path.to_path_buf(), kind, verify, sources: head[5..].iter().map(PathBuf::from).collect(), dest: PathBuf::from(&head[4]), files, done },
        records,
    })
}

/// Jobs left unfinished by an earlier run. Journals that never got past planning (nothing was
/// copied) are removed; journals still open (a running job, maybe in another window's process)
/// are left alone.
pub fn interrupted() -> Vec<Interrupted> {
    let Some(dir) = JOURNAL_DIR.get() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jrn") || crate::journal::in_use(&path) {
            continue;
        }
        match load(&path) {
            Some(l) => out.push(l.info),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort_by(|a, b| a.journal.cmp(&b.journal));
    out
}

/// Forget an interrupted job (what it copied stays).
pub fn discard(journal: &Path) {
    let _ = std::fs::remove_file(journal);
}

/// Carries an interrupted job on with its original plan: no conflict is asked again. A file is
/// skipped only when the journal says it finished, or its copy matches the source (size and
/// time; byte for byte when the job verifies fully: that happens in `finish` for every file,
/// skipped or not). Anything else at a destination is a partial copy from the cut, and is
/// replaced (through a temporary name, as always). Moves delete sources only after the check.
pub fn resume(journal: &Path, tracker: Arc<Tracker>) -> anyhow::Result<bool> {
    let Some(l) = load(journal) else {
        discard(journal);
        return Ok(true);
    };
    let tag = journal.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut p = Plan::default();
    let mut finished: HashSet<usize> = HashSet::new();
    let mut jobs: Vec<Job> = Vec::new();
    for r in &l.records {
        match (r[0].as_str(), r.len()) {
            ("D", 2) => p.dirs.push(PathBuf::from(&r[1])),
            ("N", 3) => p.new_dirs.push((PathBuf::from(&r[1]), PathBuf::from(&r[2]))),
            ("S", 2) => p.src_dirs.push(PathBuf::from(&r[1])),
            ("W", 2) => p.whole.push(PathBuf::from(&r[1])),
            ("J", 6) => jobs.push(Job {
                op: if r[1] == "r" { Op::Rename } else { Op::Copy },
                replace: r[2] == "1",
                size: r[3].parse().unwrap_or(0),
                src: PathBuf::from(&r[4]),
                dst: PathBuf::from(&r[5]),
                resume: false,
            }),
            ("F", 2) => {
                if let Ok(i) = r[1].parse() {
                    finished.insert(i);
                }
            }
            _ => {}
        }
    }
    let mut already = Vec::new();
    let mut index = Vec::new();
    for (i, mut j) in jobs.into_iter().enumerate() {
        let src_there = std::fs::symlink_metadata(&j.src).is_ok();
        let dst_there = std::fs::symlink_metadata(&j.dst).is_ok();
        match j.op {
            Op::Rename => {
                if !src_there {
                    continue; // renamed before the cut
                }
            }
            Op::Copy => {
                let tmp = part_name(&j.dst, &tag);
                if !src_there {
                    let _ = std::fs::remove_file(&tmp);
                    continue; // a move that already deleted it: its copy checked out then
                }
                if dst_there && (finished.contains(&i) || same_meta(&j.src, &j.dst)) {
                    let _ = std::fs::remove_file(&tmp);
                    already.push((j.src.clone(), j.dst.clone()));
                    continue;
                }
                if j.size >= RESTARTABLE_MIN && (if j.replace { std::fs::symlink_metadata(&tmp).is_ok() } else { dst_there }) {
                    // a big copy cut off mid-way: it carries on where it stopped
                    j.resume = true;
                } else {
                    let _ = std::fs::remove_file(&tmp);
                    // a small partial copy from the cut (or a file the plan replaces): again
                    j.replace = j.replace || dst_there;
                }
            }
        }
        index.push(i);
        p.jobs.push(j);
    }
    // a clean journal (no torn tail) to go on recording into
    let w = crate::journal::rewrite(journal, &l.records)?;
    let jr = Jr { w: Mutex::new((w, Instant::now())), path: journal.to_path_buf() };
    tracker.set_verify(l.info.verify);
    let threads = if l.rename { 4 } else { threads_for(&l.info.sources, &l.info.dest) };
    let out = run_jobs(p, &tracker, threads, Some((&jr, index)), already);
    let kind = if l.rename { Transfer::Move } else { l.info.kind };
    let r = finish(kind, &l.info.sources, &l.info.dest, &tracker, out);
    // done, failed or cancelled by the user: either way nothing is left to resume
    jr.close();
    for s in &l.info.sources {
        if let Some(p) = s.parent() {
            crate::shell::notify_updated(p);
        }
    }
    crate::shell::notify_updated(&l.info.dest);
    r
}

/// Error text for copies that didn't verify (translated by the app).
pub const NOT_VERIFIED: &str = "Doğrulanamayan dosyalar";
/// Shown while copies are read back.
pub const VERIFYING: &str = "Doğrulanıyor…";

/// Every folder under `dir` (and `dir`).
fn walk_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
        out.push(d);
    }
}

/// Sources whose copy doesn't match.
fn check(pairs: &[(PathBuf, PathBuf)], mode: Verify, tracker: &Tracker) -> Vec<PathBuf> {
    if mode == Verify::Off || pairs.is_empty() {
        return Vec::new();
    }
    if mode == Verify::Full {
        (tracker.on_progress)(1000, 0, Some(VERIFYING.to_string()));
    }
    let next = AtomicUsize::new(0);
    let bad = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    if tracker.cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((a, b)) = pairs.get(i) else { return };
                    let same = match mode {
                        Verify::Quick => same_meta(a, b),
                        _ => same_meta(a, b) && same_bytes(a, b).unwrap_or(false),
                    };
                    if !same {
                        log::warn!("fastcopy: {} does not match {}", b.display(), a.display());
                        bad.lock().unwrap().push(a.clone());
                    }
                    if mode == Verify::Full && i % 16 == 0 {
                        (tracker.on_progress)(1000, (i * 1000 / pairs.len()) as u32, None);
                    }
                }
            });
        }
    });
    bad.into_inner().unwrap()
}

/// Same size and modification time (CopyFile2 keeps it; 2 s of slack for FAT/exFAT sticks).
fn same_meta(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    match (ma.modified(), mb.modified()) {
        (Ok(x), Ok(y)) => x.duration_since(y).or_else(|_| y.duration_since(x)).is_ok_and(|d| d <= Duration::from_secs(2)),
        _ => false,
    }
}

/// Same bytes; the copy is read past the cache (what the disk holds, not what was just
/// written to memory).
fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    const CHUNK: usize = 1 << 20;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    struct Aligned(*mut u8, std::alloc::Layout);
    impl Drop for Aligned {
        fn drop(&mut self) {
            unsafe { std::alloc::dealloc(self.0, self.1) }
        }
    }
    let mut fa = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(a)?;
    let mut fb = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_NO_BUFFERING).open(b)?;
    // unbuffered reads need a sector-aligned buffer
    let layout = std::alloc::Layout::from_size_align(CHUNK, 4096).expect("layout");
    let buf = Aligned(unsafe { std::alloc::alloc(layout) }, layout);
    if buf.0.is_null() {
        return Ok(false);
    }
    let bb = unsafe { std::slice::from_raw_parts_mut(buf.0, CHUNK) };
    let mut ba = vec![0u8; CHUNK];
    loop {
        let n = fb.read(bb)?;
        let mut m = 0;
        while m < n {
            let k = fa.read(&mut ba[m..n])?;
            if k == 0 {
                return Ok(false);
            }
            m += k;
        }
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n < CHUNK {
            // the copy ended: the source must too
            return Ok(fa.read(&mut ba[..1])? == 0);
        }
    }
}

/// `inner` is `outer` or inside it, comparing like Windows does (case-insensitive, either
/// separator).
fn inside(inner: &Path, outer: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (i, o) = (norm(inner), norm(outer));
    i == o || i.strip_prefix(&o).is_some_and(|rest| rest.starts_with('\\'))
}

/// Same drive letter or same `\\server\share`: a move there is a rename.
fn same_volume(a: &Path, b: &Path) -> bool {
    use std::path::Component;
    let root = |p: &Path| match p.components().next() {
        Some(Component::Prefix(x)) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
        _ => None,
    };
    root(a).is_some() && root(a) == root(b)
}

/// Parallel copies pay off on SSDs; a spinning disk on either side only seeks more with
/// many at once, and network shares and USB sticks like a moderate number.
fn threads_for(sources: &[PathBuf], dest: &Path) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let paths = sources.iter().map(PathBuf::as_path).chain(std::iter::once(dest));
    let mut slow = false;
    // once per drive or share (5,000 files selected in one folder asked the same disk 5,000
    // times, each a volume open and a query, before the first byte)
    let mut seen = std::collections::HashSet::new();
    for p in paths {
        if !seen.insert(volume_key(p)) {
            continue;
        }
        match crate::drives::seek_penalty(p) {
            Some(true) => return 2,
            Some(false) => {}
            None => slow = true,
        }
        slow |= crate::drives::is_removable(p);
    }
    if slow { 4 } else { cores.clamp(4, 16) }
}

/// The drive (`C:`) or share (`\\server\share`) a path is on, to ask about each once.
fn volume_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\\{r}")).unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(&s).to_string());
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return s[..2].to_ascii_uppercase();
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        let mut parts = rest.split('\\');
        return format!(r"\\{}\{}", parts.next().unwrap_or(""), parts.next().unwrap_or("")).to_lowercase();
    }
    s
}

/// Whether `p` is on a local or network file system path we can copy into directly (not a
/// shell namespace, library, phone, …).
fn local_fs(p: &Path) -> bool {
    let s = p.to_string_lossy();
    (s.len() >= 3 && s.as_bytes()[1] == b':') || s.starts_with(r"\\") && !s.starts_with(r"\\?\")
}

#[cfg(test)]
mod tests {
    #[test]
    fn volume_keys_group_by_drive_and_share() {
        use super::volume_key;
        use std::path::Path;
        assert_eq!(volume_key(Path::new(r"c:\a\b.txt")), "C:");
        assert_eq!(volume_key(Path::new(r"\\?\C:\uzun\yol")), "C:");
        assert_eq!(volume_key(Path::new(r"\\NAS\Paylaşım\x")), r"\\nas\paylaşım");
        assert_eq!(volume_key(Path::new(r"\\?\UNC\nas\paylaşım\y")), r"\\nas\paylaşım");
    }

    use super::*;

    /// The same file by two spellings is one file; two files are two.
    #[test]
    fn same_file_through_another_path() {
        let dir = std::env::temp_dir().join(format!("rf-samefile-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        assert!(same_file(&a, &dir.join("sub").join("..").join("A.TXT")));
        assert!(same_file(&dir, &dir.join("sub").join("..")));
        assert!(!same_file(&a, &b));
        assert!(!same_file(&a, &dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }
    use std::sync::atomic::AtomicBool;

    fn tracker() -> Tracker {
        Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false)))
    }

    /// A copy cut off half-way (some files finished and recorded, one partial at its real name,
    /// a replace whose temporary copy was left behind), then resumed: the result is the same as
    /// an uncut copy, nothing is asked again, the user's "replace" answer still holds, and the
    /// journal is gone.
    #[test]
    fn resume_after_a_crash() {
        let b = base("resume");
        let src = b.join("kaynak");
        tree(&src);
        let dst = b.join("hedef");
        std::fs::create_dir_all(dst.join("kaynak")).unwrap();
        // an older file the user chose to replace
        std::fs::write(dst.join("kaynak/kök.txt"), b"eski").unwrap();
        set_journal_dir(std::env::temp_dir().join("rf-journals-test"));
        let t = answering(Resolve::Replace);
        let mut p = plan(std::slice::from_ref(&src), &dst, false);
        let list: Vec<Conflict> = p.conflicts.iter().map(|c| c.0.clone()).collect();
        p.resolve(&vec![Resolve::Replace; list.len()]);
        p.jobs = order(std::mem::take(&mut p.jobs));
        let jr = journal_start(Transfer::Copy, false, Verify::Full, std::slice::from_ref(&src), &dst, &p).expect("journal");
        for d in &p.dirs {
            let _ = std::fs::create_dir(d);
        }
        // the "crash": a third of the files done (most recorded), one partial, one torn temp
        let tag = jr.path.file_stem().unwrap().to_string_lossy().into_owned();
        let run = Run { tracker: &t, tag: tag.clone(), restartable: true, total: 1, done: AtomicU64::new(0), cancel: AtomicI32::new(0), clock: Mutex::new((Instant::now(), 0)), last_report: Mutex::new(Instant::now()) };
        let n = p.jobs.len();
        for (i, j) in p.jobs.iter().enumerate().take(n / 3) {
            run_job(j, &run).unwrap();
            if i % 4 != 0 {
                jr.done(i); // every 4th finished file's record is lost
            }
        }
        let partial = p.jobs[n / 3].clone();
        std::fs::write(&partial.dst, b"yarim").unwrap();
        let rep = p.jobs.iter().find(|j| j.replace).expect("the replace job").dst.clone();
        let name = rep.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(rep.with_file_name(format!(".{name}.{tag}.rfpart")), b"yarim").unwrap();
        jr.sync();
        let journal = jr.path.clone();
        drop(jr); // the process dies: the journal stays
        let found = interrupted().into_iter().find(|i| i.journal == journal).expect("the cut job is found");
        assert_eq!(found.dest, dst);
        assert!(found.done > 0 && found.done < found.files);
        // resume: nothing is asked (an `ask` that panics proves it)
        let mut t2 = tracker();
        t2.ask = Some(Box::new(|_| panic!("resume must not ask")));
        assert!(resume(&journal, Arc::new(t2)).unwrap());
        same(&src, &dst.join("kaynak"));
        assert!(!journal.exists(), "the journal is removed once the job is done");
        let leftovers = std::fs::read_dir(dst.join("kaynak")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".rfpart")).count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&b);
    }

    fn answering(r: Resolve) -> Tracker {
        let mut t = tracker();
        t.ask = Some(Box::new(move |c| Some(vec![r; c.len()])));
        t
    }

    fn base(name: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("rf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        std::fs::create_dir_all(&b).unwrap();
        b
    }

    fn tree(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("boş klasör")).unwrap();
        std::fs::write(root.join("kök.txt"), b"merhaba").unwrap();
        std::fs::write(root.join("a/boş.dat"), b"").unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(root.join("a/b/büyük.bin"), &big).unwrap();
        for i in 0..300 {
            std::fs::write(root.join(format!("a/b/c/f{i}.txt")), format!("dosya {i}").repeat(i + 1)).unwrap();
        }
    }

    fn same(a: &Path, b: &Path) {
        let ma = std::fs::metadata(a).unwrap();
        let mb = std::fs::metadata(b).unwrap();
        assert_eq!(ma.is_dir(), mb.is_dir(), "{}", b.display());
        if ma.is_dir() {
            let mut na: Vec<_> = std::fs::read_dir(a).unwrap().map(|e| e.unwrap().file_name()).collect();
            let mut nb: Vec<_> = std::fs::read_dir(b).unwrap().map(|e| e.unwrap().file_name()).collect();
            na.sort();
            nb.sort();
            assert_eq!(na, nb, "{}", b.display());
            for n in na {
                same(&a.join(&n), &b.join(&n));
            }
        } else {
            assert_eq!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap(), "{}", b.display());
            assert_eq!(ma.modified().unwrap(), mb.modified().unwrap(), "{}", b.display());
        }
    }

    fn same_bytes_tree(a: &Path, b: &Path) {
        let mut stack = vec![(a.to_path_buf(), b.to_path_buf())];
        while let Some((x, y)) = stack.pop() {
            for e in std::fs::read_dir(&x).unwrap() {
                let e = e.unwrap();
                let (p, q) = (e.path(), y.join(e.file_name()));
                if p.is_dir() {
                    stack.push((p, q));
                } else {
                    assert_eq!(std::fs::read(&p).unwrap(), std::fs::read(&q).unwrap(), "{}", q.display());
                }
            }
        }
    }

    #[test]
    fn copies_a_tree_exactly() {
        let base = base("fastcopy");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        tree(&src);
        std::fs::create_dir_all(&dst).unwrap();
        let t = tracker();
        let out = copy(&[src.clone(), src.join("kök.txt")], &dst, &t, 8);
        assert!(out.whole.is_empty() && out.failed.is_empty());
        assert_eq!(out.undo.len(), 2);
        same(&src, &dst.join("kaynak"));
        same(&src.join("kök.txt"), &dst.join("kök.txt"));
        // again into the same place, nobody asked: the folder merges, every file gets a twin
        let out = copy(&[src.clone()], &dst, &t, 8);
        assert!(out.whole.is_empty());
        assert!(dst.join("kaynak/kök (2).txt").exists());
        assert!(dst.join("kaynak/a/b/c/f7 (2).txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn conflicts_replace_skip_keep_both() {
        let base = base("conflicts");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        std::fs::create_dir_all(src.join("alt")).unwrap();
        std::fs::create_dir_all(dst.join("kaynak/alt")).unwrap();
        std::fs::write(src.join("alt/aynı.txt"), b"yeni").unwrap();
        std::fs::write(src.join("alt/yeni.txt"), b"sadece kaynakta").unwrap();
        std::fs::write(dst.join("kaynak/alt/aynı.txt"), b"eski").unwrap();
        std::fs::write(dst.join("kaynak/alt/sadece hedefte.txt"), "kalır".as_bytes()).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        // skip: the old file stays, the new one arrives, the merge keeps the rest
        let mut t = tracker();
        let a2 = asked.clone();
        t.ask = Some(Box::new(move |c| {
            a2.lock().unwrap().extend(c.iter().map(|c| c.dst.clone()));
            Some(vec![Resolve::Skip; c.len()])
        }));
        transfer(Transfer::Copy, &[src.clone()], &dst, Arc::new(t)).unwrap();
        assert_eq!(*asked.lock().unwrap(), vec![dst.join("kaynak/alt/aynı.txt")], "only the clashing file is asked");
        assert_eq!(std::fs::read(dst.join("kaynak/alt/aynı.txt")).unwrap(), b"eski");
        assert_eq!(std::fs::read(dst.join("kaynak/alt/yeni.txt")).unwrap(), b"sadece kaynakta");
        assert!(dst.join("kaynak/alt/sadece hedefte.txt").exists());
        // keep both
        transfer(Transfer::Copy, &[src.clone()], &dst, Arc::new(answering(Resolve::KeepBoth))).unwrap();
        assert_eq!(std::fs::read(dst.join("kaynak/alt/aynı (2).txt")).unwrap(), b"yeni");
        assert_eq!(std::fs::read(dst.join("kaynak/alt/aynı.txt")).unwrap(), b"eski");
        // replace (a read-only target included), no temporary file left behind
        let target = dst.join("kaynak/alt/aynı.txt");
        let mut perm = std::fs::metadata(&target).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&target, perm).unwrap();
        transfer(Transfer::Copy, &[src.clone()], &dst, Arc::new(answering(Resolve::Replace))).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"yeni");
        let leftovers = std::fs::read_dir(dst.join("kaynak/alt")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".rfpart")).count();
        assert_eq!(leftovers, 0);
        // cancel at the question: nothing written
        let mut t = tracker();
        t.ask = Some(Box::new(|_| None));
        std::fs::write(src.join("alt/başka.txt"), b"x").unwrap();
        assert!(!transfer(Transfer::Copy, &[src.clone()], &dst, Arc::new(t)).unwrap());
        assert!(!dst.join("kaynak/alt/başka.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn keep_both_never_takes_a_planned_name() {
        let base = base("freename");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        // "a (2).txt" is copied in this same run; the clashing "a.txt" must become "a (3).txt"
        std::fs::write(src.join("a.txt"), b"yeni a").unwrap();
        std::fs::write(src.join("a (2).txt"), b"kaynaktaki a (2)").unwrap();
        std::fs::write(dst.join("a.txt"), b"eski a").unwrap();
        transfer(Transfer::Copy, &[src.join("a.txt"), src.join("a (2).txt")], &dst, Arc::new(answering(Resolve::KeepBoth))).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"eski a");
        assert_eq!(std::fs::read(dst.join("a (2).txt")).unwrap(), b"kaynaktaki a (2)");
        assert_eq!(std::fs::read(dst.join("a (3).txt")).unwrap(), b"yeni a");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn moves_on_one_volume_rename_and_merge() {
        let base = base("rename");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        tree(&src);
        std::fs::create_dir_all(dst.join("kaynak/a")).unwrap();
        std::fs::write(dst.join("kaynak/kök.txt"), b"eski").unwrap();
        std::fs::write(dst.join("kaynak/a/hedefte.txt"), "kalır".as_bytes()).unwrap();
        let t = Arc::new(answering(Resolve::Skip));
        assert!(transfer(Transfer::Move, &[src.clone()], &dst, t.clone()).unwrap());
        // merged: the clash skipped (stays in the source), the rest moved
        assert_eq!(std::fs::read(dst.join("kaynak/kök.txt")).unwrap(), b"eski");
        assert_eq!(std::fs::read(src.join("kök.txt")).unwrap(), b"merhaba");
        assert!(dst.join("kaynak/a/b/c/f299.txt").exists());
        assert!(dst.join("kaynak/a/hedefte.txt").exists());
        assert!(!src.join("a").exists(), "emptied folders go");
        assert!(src.exists(), "the source keeps the skipped file");
        // undo pairs name what was moved, never the merged folder itself
        let res = t.results.lock().unwrap().clone();
        assert!(!res.iter().any(|(_, d)| *d == dst.join("kaynak")), "{res:?}");
        // a plain rename of a whole folder
        let lone = base.join("tek");
        std::fs::create_dir_all(lone.join("iç")).unwrap();
        assert!(transfer(Transfer::Move, &[lone.clone()], &dst, Arc::new(tracker())).unwrap());
        assert!(!lone.exists() && dst.join("tek/iç").is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_deletes_sources_only_after_everything_arrived() {
        let base = base("fastmove");
        let (src, keep, dst) = (base.join("kaynak"), base.join("kopya-referans"), base.join("hedef"));
        tree(&src);
        tree(&keep);
        std::fs::create_dir_all(&dst).unwrap();
        let lone = base.join("tek dosya.txt");
        std::fs::write(&lone, b"tek").unwrap();
        // the cross-volume path, exercised on one volume
        let t = Arc::new(tracker());
        let (out, jr) = execute(Transfer::Move, &[src.clone(), lone.clone()], &dst, &t, 8, false);
        assert!(finish(Transfer::Move, &[src.clone(), lone.clone()], &dst, &t, out).unwrap());
        if let Some(j) = jr {
            j.close();
        }
        assert!(!src.exists() && !lone.exists(), "sources removed");
        same_bytes_tree(&keep, &dst.join("kaynak"));
        assert_eq!(std::fs::read(dst.join("tek dosya.txt")).unwrap(), b"tek");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn verify_catches_a_changed_copy() {
        let base = base("verify");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        tree(&src);
        std::fs::create_dir_all(&dst).unwrap();
        let t = tracker();
        t.set_verify(Verify::Full);
        assert!(transfer(Transfer::Copy, &[src.clone()], &dst, Arc::new(t)).unwrap(), "a clean copy verifies");
        let pairs = vec![(src.join("a/b/büyük.bin"), dst.join("kaynak/a/b/büyük.bin"))];
        assert!(check(&pairs, Verify::Full, &tracker()).is_empty());
        // flip one byte in the middle, keep size and time
        let f = dst.join("kaynak/a/b/büyük.bin");
        let time = std::fs::metadata(&f).unwrap().modified().unwrap();
        let mut bytes = std::fs::read(&f).unwrap();
        bytes[1_500_000] ^= 0xFF;
        std::fs::write(&f, &bytes).unwrap();
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(time).unwrap();
        assert!(check(&pairs, Verify::Quick, &tracker()).is_empty(), "size and time still match");
        assert_eq!(check(&pairs, Verify::Full, &tracker()), vec![src.join("a/b/büyük.bin")]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pause_holds_and_speed_limit_slows() {
        let base = base("pause");
        let src = base.join("kaynak");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(base.join("hedef")).unwrap();
        std::fs::write(src.join("2mb.bin"), vec![1u8; 2 << 20]).unwrap();
        // 4 MB/s limit: 2 MB take ~0.5 s
        let t = tracker();
        t.limit.store(4 << 20, Ordering::Relaxed);
        let start = Instant::now();
        copy(&[src.join("2mb.bin")], &base.join("hedef"), &t, 2);
        assert!(start.elapsed() >= Duration::from_millis(350), "{:?}", start.elapsed());
        // paused: nothing happens until resumed
        let t = Arc::new(tracker());
        t.paused.store(true, Ordering::Relaxed);
        let t2 = t.clone();
        let (s2, d2) = (src.clone(), base.join("hedef2"));
        std::fs::create_dir_all(&d2).unwrap();
        let h = std::thread::spawn(move || copy(&[s2.join("2mb.bin")], &d2, &t2, 2).undo.len());
        std::thread::sleep(Duration::from_millis(400));
        assert!(!base.join("hedef2/2mb.bin").exists(), "nothing copied while paused");
        t.paused.store(false, Ordering::Relaxed);
        assert_eq!(h.join().unwrap(), 1);
        assert!(base.join("hedef2/2mb.bin").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn order_spreads_small_files_over_folders() {
        let j = |d: &str, f: &str, size: u64| Job { src: PathBuf::from(format!("s/{d}/{f}")), dst: PathBuf::from(format!("d/{d}/{f}")), size, op: Op::Copy, replace: false, resume: false };
        let jobs = vec![j("a", "1", 10), j("a", "2", 10), j("a", "3", 10), j("b", "1", 10), j("b", "2", 10), j("c", "big", 100 << 20)];
        let order: Vec<String> = order(jobs).iter().map(|j| j.dst.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(order, ["d/c/big", "d/a/1", "d/b/1", "d/a/2", "d/b/2", "d/a/3"]);
    }

    #[test]
    fn inside_is_case_insensitive() {
        assert!(inside(Path::new(r"c:\a\alt"), Path::new(r"C:\A")));
        assert!(inside(Path::new(r"C:\A"), Path::new(r"c:/a/")));
        assert!(!inside(Path::new(r"C:\ab"), Path::new(r"C:\a")));
    }

    #[test]
    fn volumes_and_self_nesting() {
        assert!(same_volume(Path::new(r"C:\a"), Path::new(r"c:\b\c")));
        assert!(!same_volume(Path::new(r"C:\a"), Path::new(r"D:\a")));
        assert!(same_volume(Path::new(r"\\nas\paylaşım\a"), Path::new(r"\\NAS\paylaşım\b")));
        assert!(!same_volume(Path::new(r"\\nas\a\x"), Path::new(r"\\nas\b\x")));
        // a folder into its own subfolder is left to the shell (which refuses it)
        let base = base("fastnest");
        std::fs::create_dir_all(base.join("a/alt")).unwrap();
        let t = tracker();
        let out = copy(&[base.join("a")], &base.join("a/alt"), &t, 2);
        assert!(out.undo.is_empty() && out.whole == vec![base.join("a")]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cancel_stops_without_partial_files() {
        let base = base("fastcopy-c");
        let (src, dst) = (base.join("kaynak"), base.join("hedef"));
        tree(&src);
        std::fs::create_dir_all(&dst).unwrap();
        let t = tracker();
        t.cancel.store(true, Ordering::Relaxed);
        let _ = copy(&[src.clone()], &dst, &t, 4);
        // whatever exists is complete: no file differs from its source
        let mut stack = vec![dst.join("kaynak")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let rel = p.strip_prefix(dst.join("kaynak")).unwrap();
                    assert_eq!(std::fs::read(&p).unwrap(), std::fs::read(src.join(rel)).unwrap());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
