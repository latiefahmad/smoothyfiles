//! Disk usage: where the space under a folder goes (the "Show disk usage" view).
//!
//! The folder is walked in parallel by a few below-normal-priority rayon threads (the same
//! recipe as the name index and the folder sizes: `FindFirstFileExW`, reparse points neither
//! counted nor followed, unreadable folders skipped) and kept as a compact struct-of-arrays
//! tree: about 60 bytes and one name per entry, so a million files cost well under 100 MB.
//!
//! Every directory's entries get a contiguous run of ids, reserved when the directory is
//! listed. A parent is therefore always listed before its children, its id is smaller than
//! theirs, and one backwards pass over the ids adds every entry to its parent: folder totals
//! need no recursion. A directory's children are a plain id range.
//!
//! Hard links are counted once per link (like the folder sizes); both the logical size and the
//! size on disk (rounded up to the volume's clusters) are kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use rf_core::attrs;

use crate::search::for_each_raw;
use crate::wide::from_wide;

const DIR: u8 = 1;
/// No parent: the root.
const NONE: u32 = u32::MAX;

/// Which size ranks and measures the entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Metric {
    /// What the files hold.
    #[default]
    Logical,
    /// What they take on the disk (whole clusters).
    OnDisk,
}

/// What the walk has found so far, readable from any thread while it runs.
#[derive(Default)]
pub struct Progress {
    pub entries: AtomicU64,
    pub bytes: AtomicU64,
}

/// The finished tree. Id 0 is the folder that was walked.
pub struct DiskTree {
    root: PathBuf,
    parent: Vec<u32>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    size: Vec<u64>,
    /// A folder's place in the folder-only arrays below; `NONE` for files. Most entries are
    /// files, which need neither totals nor a child range: 20 bytes less for each of them.
    slot: Vec<u32>,
    /// Per folder (by slot): size on disk, files below, children `first..first + count`.
    dir_alloc: Vec<u64>,
    dir_files: Vec<u32>,
    dir_first: Vec<u32>,
    dir_count: Vec<u32>,
    /// A file's size on disk is its size rounded up to whole clusters, worked out when asked.
    cluster: Option<u64>,
    flags: Vec<u8>,
    names: Vec<u8>,
    /// Folders that could not be listed (access denied and the like).
    skipped: u32,
    cancelled: bool,
}

/// One listed directory, waiting to be put into the arrays.
struct Chunk {
    base: u32,
    parent: u32,
    names: Vec<u8>,
    lens: Vec<u16>,
    sizes: Vec<u64>,
    flags: Vec<u8>,
}

struct Build<'a> {
    next: &'a AtomicU32,
    chunks: &'a Mutex<Vec<Chunk>>,
    cancel: &'a AtomicBool,
    progress: &'a Progress,
    skipped: &'a AtomicU32,
}

fn walk<'s>(sc: &rayon::Scope<'s>, b: &'s Build<'s>, dir: PathBuf, id: u32) {
    if b.cancel.load(Ordering::Relaxed) {
        return;
    }
    let (mut names, mut lens, mut sizes, mut flags, mut subs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut bytes, mut entries) = (0u64, 0u64);
    let r = for_each_raw(&dir, |d| {
        let n = &d.cFileName;
        if n[0] == b'.' as u16 && (n[1] == 0 || (n[1] == b'.' as u16 && n[2] == 0)) {
            return;
        }
        let a = d.dwFileAttributes;
        if a & attrs::REPARSE_POINT != 0 {
            return;
        }
        let name = from_wide(n);
        let is_dir = a & attrs::DIRECTORY != 0;
        let size = if is_dir { 0 } else { ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64 };
        if is_dir {
            subs.push((flags.len() as u32, name.clone()));
        }
        lens.push(name.len().min(u16::MAX as usize) as u16);
        names.extend_from_slice(&name.as_bytes()[..name.len().min(u16::MAX as usize)]);
        sizes.push(size);
        flags.push(if is_dir { DIR } else { 0 });
        bytes += size;
        entries += 1;
    });
    if r.is_err() {
        b.skipped.fetch_add(1, Ordering::Relaxed);
    }
    if lens.is_empty() {
        return;
    }
    b.progress.entries.fetch_add(entries, Ordering::Relaxed);
    b.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    let base = b.next.fetch_add(lens.len() as u32, Ordering::Relaxed);
    for (k, name) in subs {
        let sub = dir.join(name);
        let child = base + k;
        sc.spawn(move |sc| walk(sc, b, sub, child));
    }
    b.chunks.lock().push(Chunk { base, parent: id, names, lens, sizes, flags });
}

fn pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 8))
        .thread_name(|i| format!("rf-diskusage-{i}"))
        .start_handler(|_| unsafe {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        })
        .build()
        .expect("disk usage thread pool")
}

impl DiskTree {
    /// Walks `root` (blocking; `cancel` stops it early, and the tree then holds what was found
    /// so far, see [`DiskTree::cancelled`]). `progress` is updated as directories are listed.
    pub fn build(root: &Path, cancel: &AtomicBool, progress: &Progress) -> DiskTree {
        let next = AtomicU32::new(1);
        let chunks = Mutex::new(Vec::new());
        let skipped = AtomicU32::new(0);
        let b = Build { next: &next, chunks: &chunks, cancel, progress, skipped: &skipped };
        pool().scope(|sc| walk(sc, &b, root.to_path_buf(), 0));
        let total = next.load(Ordering::Relaxed) as usize;
        let mut chunks = chunks.into_inner();
        chunks.sort_unstable_by_key(|c| c.base);
        let cluster = crate::foldersize::cluster_size(root);
        let dirs = 1 + chunks.iter().map(|c| c.flags.iter().filter(|&&f| f & DIR != 0).count()).sum::<usize>();
        let mut t = DiskTree {
            root: root.to_path_buf(),
            parent: vec![NONE; total],
            name_off: vec![0; total],
            name_len: vec![0; total],
            size: vec![0; total],
            slot: vec![NONE; total],
            dir_alloc: Vec::with_capacity(dirs),
            dir_files: Vec::with_capacity(dirs),
            dir_first: Vec::with_capacity(dirs),
            dir_count: Vec::with_capacity(dirs),
            cluster,
            flags: vec![0; total],
            names: Vec::with_capacity(chunks.iter().map(|c| c.names.len()).sum()),
            skipped: skipped.load(Ordering::Relaxed),
            cancelled: cancel.load(Ordering::Relaxed),
        };
        t.flags[0] = DIR;
        t.new_slot(0);
        // chunks in id order: a folder's own entry (in its parent's listing) is laid out, and
        // gets its slot, before its listing comes
        for c in chunks {
            let ps = t.slot[c.parent as usize] as usize;
            t.dir_count[ps] = c.lens.len() as u32;
            t.dir_first[ps] = c.base;
            let mut off = 0usize;
            for (k, (&len, &f)) in c.lens.iter().zip(&c.flags).enumerate() {
                let i = c.base as usize + k;
                t.parent[i] = c.parent;
                t.name_off[i] = t.names.len() as u32;
                t.name_len[i] = len;
                t.flags[i] = f;
                t.names.extend_from_slice(&c.names[off..off + len as usize]);
                off += len as usize;
                if f & DIR == 0 {
                    t.size[i] = c.sizes[k];
                } else {
                    t.new_slot(i as u32);
                }
            }
        }
        // ids reserved by a cancelled walk but never filled keep parent NONE and size 0; a
        // child's id is always greater than its parent's, so one pass backwards totals everything
        for i in (1..total).rev() {
            let p = t.parent[i];
            if p != NONE {
                let ps = t.slot[p as usize] as usize;
                let (size, alloc, files) = (t.size[i], t.allocated(i as u32), t.files(i as u32));
                t.size[p as usize] += size;
                t.dir_alloc[ps] += alloc;
                t.dir_files[ps] = t.dir_files[ps].saturating_add(files);
            }
        }
        t
    }

    fn new_slot(&mut self, i: u32) {
        self.slot[i as usize] = self.dir_alloc.len() as u32;
        self.dir_alloc.push(0);
        self.dir_files.push(0);
        self.dir_first.push(0);
        self.dir_count.push(0);
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Entries, the root included.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.len() <= 1
    }

    /// The walk was stopped before it finished.
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Folders that could not be read.
    pub fn skipped(&self) -> u32 {
        self.skipped
    }

    /// Bytes held (arrays and names).
    pub fn memory(&self) -> usize {
        let n = self.parent.capacity();
        n * (4 + 4 + 2 + 8 + 4 + 1) + self.dir_alloc.capacity() * (8 + 4 + 4 + 4) + self.names.capacity()
    }

    pub fn name(&self, i: u32) -> &str {
        let (o, l) = (self.name_off[i as usize] as usize, self.name_len[i as usize] as usize);
        // names are written from Strings; a name cut at the length limit may end inside a
        // character, which reads as empty rather than panicking
        std::str::from_utf8(&self.names[o..o + l]).unwrap_or("")
    }

    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & DIR != 0
    }

    pub fn parent(&self, i: u32) -> Option<u32> {
        Some(self.parent[i as usize]).filter(|&p| p != NONE)
    }

    pub fn size(&self, i: u32) -> u64 {
        self.size[i as usize]
    }

    pub fn allocated(&self, i: u32) -> u64 {
        match self.slot[i as usize] {
            NONE => {
                let s = self.size[i as usize];
                self.cluster.map_or(s, |cl| s.div_ceil(cl) * cl)
            }
            d => self.dir_alloc[d as usize],
        }
    }

    pub fn weight(&self, i: u32, m: Metric) -> u64 {
        match m {
            Metric::Logical => self.size(i),
            Metric::OnDisk => self.allocated(i),
        }
    }

    /// Files below `i` (1 for a file).
    pub fn files(&self, i: u32) -> u32 {
        match self.slot[i as usize] {
            // a file counts itself; a reserved id a cancelled walk never filled counts nothing
            NONE => (self.parent[i as usize] != NONE) as u32,
            d => self.dir_files[d as usize],
        }
    }

    /// The ids of `i`'s direct children (empty for files and empty folders).
    pub fn children(&self, i: u32) -> std::ops::Range<u32> {
        match self.slot[i as usize] {
            NONE => 0..0,
            d => {
                let f = self.dir_first[d as usize];
                f..f + self.dir_count[d as usize]
            }
        }
    }

    /// `i`'s children, biggest first (ties by name, plain byte order: this runs on every redraw
    /// of a big folder).
    pub fn sorted_children(&self, i: u32, m: Metric) -> Vec<u32> {
        let mut v: Vec<u32> = self.children(i).collect();
        v.sort_unstable_by(|&a, &b| self.weight(b, m).cmp(&self.weight(a, m)).then_with(|| self.name(a).cmp(self.name(b))));
        v
    }

    /// Full path of `i`.
    pub fn path(&self, i: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = i;
        while let Some(p) = self.parent(cur) {
            parts.push(cur);
            cur = p;
        }
        let mut path = self.root.clone();
        for &c in parts.iter().rev() {
            path.push(self.name(c));
        }
        path
    }

    /// Id of the entry at `path` if it lies in the tree.
    pub fn find(&self, path: &Path) -> Option<u32> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut cur = 0u32;
        for comp in rel.components() {
            let want = comp.as_os_str().to_string_lossy();
            cur = self.children(cur).find(|&c| self.name(c).eq_ignore_ascii_case(&want))?;
        }
        Some(cur)
    }
}

/// A walk running in the background. Dropping it (closing the tab) stops the walk.
pub struct DiskScan {
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    result: Arc<Mutex<Option<DiskTree>>>,
}

impl DiskScan {
    pub fn start(root: PathBuf) -> DiskScan {
        let scan = DiskScan { cancel: Arc::default(), progress: Arc::default(), result: Arc::default() };
        let (cancel, progress, result) = (scan.cancel.clone(), scan.progress.clone(), scan.result.clone());
        let _ = std::thread::Builder::new().name("rf-diskusage".into()).spawn(move || {
            let t = DiskTree::build(&root, &cancel, &progress);
            *result.lock() = Some(t);
        });
        scan
    }

    /// (entries, bytes) found so far.
    pub fn progress(&self) -> (u64, u64) {
        (self.progress.entries.load(Ordering::Relaxed), self.progress.bytes.load(Ordering::Relaxed))
    }

    /// The tree once the walk has finished (taken: later calls return `None`).
    pub fn take(&self) -> Option<DiskTree> {
        self.result.lock().take()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for DiskScan {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rf-diskusage-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/deep")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("top.txt"), vec![1u8; 100]).unwrap();
        fs::write(root.join("a/one.bin"), vec![1u8; 1000]).unwrap();
        fs::write(root.join("a/deep/two.mp4"), vec![1u8; 5000]).unwrap();
        fs::write(root.join("a/deep/three.dll"), vec![1u8; 20]).unwrap();
        fs::write(root.join("b/four.zip"), vec![1u8; 300]).unwrap();
        root
    }

    fn build(root: &Path) -> DiskTree {
        DiskTree::build(root, &AtomicBool::new(false), &Progress::default())
    }

    #[test]
    fn totals_are_subtree_sums() {
        let root = temp_tree("totals");
        let progress = Progress::default();
        let t = DiskTree::build(&root, &AtomicBool::new(false), &progress);
        // 4 folders (a, deep, b, empty) + 5 files + the root
        assert_eq!(t.len(), 10);
        assert_eq!(progress.entries.load(Ordering::Relaxed), 9);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 6420);
        assert_eq!(t.size(0), 6420);
        assert_eq!(t.files(0), 5);
        let a = t.find(&root.join("a")).unwrap();
        assert_eq!(t.size(a), 6020);
        assert_eq!(t.files(a), 3);
        let deep = t.find(&root.join("a").join("deep")).unwrap();
        assert_eq!(t.size(deep), 5020);
        assert_eq!(t.parent(deep), Some(a));
        assert_eq!(t.path(deep), root.join("a").join("deep"));
        let empty = t.find(&root.join("empty")).unwrap();
        assert!(t.is_dir(empty) && t.size(empty) == 0 && t.children(empty).is_empty());
        // children sum to the parent, on disk never below the logical size
        for i in 0..t.len() as u32 {
            if t.is_dir(i) {
                assert_eq!(t.children(i).map(|c| t.size(c)).sum::<u64>(), t.size(i));
                assert_eq!(t.children(i).map(|c| t.allocated(c)).sum::<u64>(), t.allocated(i));
            }
            assert!(t.allocated(i) >= t.size(i));
        }
        let order: Vec<&str> = t.sorted_children(0, Metric::Logical).into_iter().map(|c| t.name(c)).collect();
        assert_eq!(order, ["a", "b", "top.txt", "empty"]);
        assert!(t.memory() > 0 && !t.cancelled());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_folder_is_an_empty_tree() {
        let t = build(Path::new(r"C:\rf-diskusage-no-such-folder"));
        assert!(t.is_empty());
        assert_eq!(t.size(0), 0);
        assert_eq!(t.skipped(), 1);
    }

    #[test]
    fn cancel_stops_the_walk() {
        let root = temp_tree("cancel");
        let t = DiskTree::build(&root, &AtomicBool::new(true), &Progress::default());
        assert!(t.cancelled());
        assert!(t.len() <= 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dropping_a_scan_cancels_and_a_scan_finishes() {
        let root = temp_tree("scan");
        let scan = DiskScan::start(root.clone());
        let mut tree = None;
        for _ in 0..200 {
            tree = scan.take();
            if tree.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(tree.expect("scan finished").size(0), 6420);
        assert_eq!(scan.progress().0, 9);
        drop(scan);
        let _ = fs::remove_dir_all(&root);
    }
}
