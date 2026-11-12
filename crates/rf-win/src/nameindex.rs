//! A name index of whole folders or drives: searching it takes milliseconds where walking the
//! disk takes minutes (6.5 million entries in a user folder: 40 s).
//!
//! **Layout.** One node per file or folder, struct-of-arrays: parent id, name id, attributes:
//! 9 bytes per entry. Names are kept once each (UTF-8, the real spelling) in a name table, as
//! most of them repeat: a developer's drive holds 4.8 million entries with 490 thousand
//! different names (94 MB of names, 13 MB once each). Node 0 is the root folder itself. Ids
//! follow the walk, so a folder's entries are adjacent.
//!
//! **Building** walks the root in parallel (below-normal priority, like the search walk), not
//! entering reparse points. Workers reserve id ranges from a counter and hand their listings to
//! the caller, which lays them out by id once the walk ends.
//!
//! **Searching** matches each different name once, then picks the entries carrying a matching
//! one, both on all cores: ASCII names are compared case-insensitively
//! without allocating; names or patterns with other letters go through the same folding the walk
//! search uses (Turkish İ/ı included). A hit counts only if no folder above it was removed (and,
//! unless hidden files are wanted, none is hidden), so removing a folder never has to touch its
//! contents.
//!
//! **Keeping up** ([`Index::refresh_dirs`]): a changed folder is listed again and compared with
//! its indexed entries by name: gone ones are marked removed, new ones appended (new folders
//! walked), the rest updated. [`Index::compact`] drops removed entries and their contents.
//!
//! **Saved** to a file ([`Index::save`] / [`Index::load`]) so a start doesn't walk again. With
//! it go the folders' file ids and the volume's change journal position: at the next start,
//! [`Index::catch_up`] lists again only the folders the journal says changed while the app was
//! closed (see [`crate::usn`]); a whole walk is needed only when the journal can't tell.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rf_core::attrs;

use crate::search::{Matcher, for_each_raw};

const NONE: u32 = u32::MAX;
const DIR: u8 = 1;
const HIDDEN: u8 = 2;
const REPARSE: u8 = 4;
const REMOVED: u8 = 128;

/// A hash set of ids whose keys live elsewhere (in the index arrays): 4 bytes a slot instead of
/// a stored key. Open addressing with linear probing; the caller hashes and compares through
/// the ids, and gives the hash of any id when slots move (growing, removing).
struct IdTable {
    slots: Vec<u32>,
    len: usize,
}

impl IdTable {
    fn new() -> Self {
        IdTable { slots: Vec::new(), len: 0 }
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// The id stored for `hash` that `is` accepts.
    fn find(&self, hash: u64, is: impl Fn(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mut at = hash as usize & self.mask();
        loop {
            match self.slots[at] {
                NONE => return None,
                id if is(id) => return Some(id),
                _ => at = (at + 1) & self.mask(),
            }
        }
    }

    /// Adds `id` (not already in) under `hash`; `hash_of` rehashes the others when growing.
    fn insert(&mut self, hash: u64, id: u32, hash_of: impl Fn(u32) -> u64) {
        if (self.len + 1) * 8 > self.slots.len() * 7 {
            self.grow((self.len + 1).max(16), &hash_of);
        }
        let mut at = hash as usize & self.mask();
        while self.slots[at] != NONE {
            at = (at + 1) & self.mask();
        }
        self.slots[at] = id;
        self.len += 1;
    }

    /// Room for `n` ids (power-of-two slots, at most 7/8 full).
    fn grow(&mut self, n: usize, hash_of: &impl Fn(u32) -> u64) {
        let size = (n * 8 / 7 + 1).next_power_of_two().max(16);
        if size <= self.slots.len() {
            return;
        }
        let old = std::mem::replace(&mut self.slots, vec![NONE; size]);
        let mask = size - 1;
        for id in old.into_iter().filter(|&id| id != NONE) {
            let mut at = hash_of(id) as usize & mask;
            while self.slots[at] != NONE {
                at = (at + 1) & mask;
            }
            self.slots[at] = id;
        }
    }

    /// Takes `id` out (found under `hash`), moving later entries of its run back so lookups
    /// never stop at a hole.
    fn remove(&mut self, hash: u64, id: u32, hash_of: impl Fn(u32) -> u64) {
        if self.slots.is_empty() {
            return;
        }
        let mask = self.mask();
        let mut at = hash as usize & mask;
        loop {
            match self.slots[at] {
                NONE => return,
                x if x == id => break,
                _ => at = (at + 1) & mask,
            }
        }
        self.slots[at] = NONE;
        self.len -= 1;
        let mut hole = at;
        let mut next = (at + 1) & mask;
        while self.slots[next] != NONE {
            let home = hash_of(self.slots[next]) as usize & mask;
            // the entry may fill the hole if its home isn't between the hole and its place
            let between = if hole <= next { hole < home && home <= next } else { hole < home || home <= next };
            if !between {
                self.slots[hole] = self.slots[next];
                self.slots[next] = NONE;
                hole = next;
            }
            next = (next + 1) & mask;
        }
    }

    fn clear(&mut self) {
        self.slots = Vec::new();
        self.len = 0;
    }

    fn memory(&self) -> usize {
        self.slots.capacity() * 4
    }
}

pub struct Index {
    root: PathBuf,
    parent: Vec<u32>,
    /// Each entry's name, an id into `names`.
    name_id: Vec<u32>,
    flags: Vec<u8>,
    names: Names,
    /// Folders by (parent, folded name): finding a folder from a path when it changes.
    dirs: IdTable,
    removed: usize,
    /// Folders by file id, as the change journal names them.
    frn: HashMap<u64, u32>,
    /// The change journal (id, next record) this index is current with.
    journal: Option<(u64, i64)>,
    /// Changes when ids do (compaction): a refresh worked out before then is not applied.
    generation: u64,
}

/// Every different name once: UTF-8 in one pool, name `k` is `pool[off[k]..off[k + 1]]`.
struct Names {
    pool: Vec<u8>,
    off: Vec<u32>,
    /// Name ids by the hash of their bytes.
    lookup: IdTable,
}

impl Names {
    fn new() -> Self {
        Names { pool: Vec::new(), off: vec![0], lookup: IdTable::new() }
    }

    fn len(&self) -> usize {
        self.off.len() - 1
    }

    fn get(&self, k: u32) -> &[u8] {
        &self.pool[self.off[k as usize] as usize..self.off[k as usize + 1] as usize]
    }

    fn hash(name: &[u8]) -> u64 {
        // FNV-1a: short names, no allocation, good spread over the table's low bits once mixed
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in name {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^ (h >> 29)
    }

    /// The id of `name`, added if new.
    fn intern(&mut self, name: &[u8]) -> u32 {
        let h = Self::hash(name);
        let (pool, off) = (&self.pool, &self.off);
        let get = |k: u32| &pool[off[k as usize] as usize..off[k as usize + 1] as usize];
        if let Some(k) = self.lookup.find(h, |k| get(k) == name) {
            return k;
        }
        let k = self.len() as u32;
        self.pool.extend_from_slice(name);
        self.off.push(self.pool.len() as u32);
        let (pool, off) = (&self.pool, &self.off);
        self.lookup.insert(h, k, |j| Self::hash(&pool[off[j as usize] as usize..off[j as usize + 1] as usize]));
        k
    }

    /// The lookup of names read from a file.
    fn rebuild_lookup(&mut self) {
        let (pool, off) = (&self.pool, &self.off);
        let get = |k: u32| &pool[off[k as usize] as usize..off[k as usize + 1] as usize];
        let mut t = IdTable::new();
        t.grow(self.off.len() - 1, &|j| Self::hash(get(j)));
        for k in 0..(self.off.len() - 1) as u32 {
            let name = get(k);
            let h = Self::hash(name);
            // the same name stored twice: the first one serves
            if t.find(h, |j| get(j) == name).is_none() {
                t.insert(h, k, |j| Self::hash(get(j)));
            }
        }
        self.lookup = t;
    }

    fn memory(&self) -> usize {
        self.pool.capacity() + self.off.capacity() * 4 + self.lookup.memory()
    }

    fn shrink(&mut self) {
        self.pool.shrink_to_fit();
        self.off.shrink_to_fit();
    }
}

/// One folder's listing from a build worker: entries `base..base + ids.len()`, their names
/// already in the shared name table (raw names of a whole drive would be held twice).
struct Chunk {
    base: u32,
    parent: u32,
    ids: Vec<u32>,
    flags: Vec<u8>,
    /// (entry within the chunk, file id) of its folders
    frns: Vec<(u32, u64)>,
}

fn flags_of(a: u32) -> u8 {
    let mut f = 0;
    if a & attrs::DIRECTORY != 0 {
        f |= DIR;
    }
    if a & attrs::HIDDEN != 0 {
        f |= HIDDEN;
    }
    if a & attrs::REPARSE_POINT != 0 {
        f |= REPARSE;
    }
    f
}

fn fold_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        c.hash(&mut h);
    }
    h.finish()
}

/// Measuring only (`examples/walk_ids.rs`): list without file ids, as before they were kept.
static PLAIN_LISTING: AtomicBool = AtomicBool::new(false);

#[doc(hidden)]
pub fn bench_plain_listing(on: bool) {
    PLAIN_LISTING.store(on, Ordering::Relaxed);
}

/// One folder's listing: names (UTF-8, one pool), their lengths, flags, file ids.
struct Listing {
    names: Vec<u8>,
    lens: Vec<u16>,
    flags: Vec<u8>,
    ids: Vec<u64>,
}

/// Lists `dir`: every entry but `.` and `..`, with its file id (what the change journal knows a
/// folder by). Read with `GetFileInformationByHandleEx` in 64 KB batches; where that isn't
/// available (some network or FAT volumes), `FindFirstFileEx` without ids.
fn list(dir: &Path) -> Listing {
    let mut out = Listing { names: Vec::new(), lens: Vec::new(), flags: Vec::new(), ids: Vec::new() };
    let mut push = |name: &[u16], attrs: u32, id: u64| {
        if name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16] {
            return;
        }
        let s = String::from_utf16_lossy(name);
        out.names.extend_from_slice(s.as_bytes());
        out.lens.push(s.len().min(u16::MAX as usize) as u16);
        out.flags.push(flags_of(attrs));
        out.ids.push(id);
    };
    if PLAIN_LISTING.load(Ordering::Relaxed) || list_with_ids(dir, &mut push).is_none() {
        let _ = for_each_raw(dir, |d| {
            let n = &d.cFileName;
            let end = n.iter().position(|&c| c == 0).unwrap_or(n.len());
            push(&n[..end], d.dwFileAttributes, 0);
        });
    }
    out
}

/// A refresh worked out from a read of the index ([`Index::plan_refresh`]).
pub struct RefreshPlan {
    generation: u64,
    dirs: Vec<PlanDir>,
}

struct PlanDir {
    node: u32,
    path: PathBuf,
    kids: Vec<Kid>,
}

struct Kid {
    id: u32,
    name: String,
    flags: u8,
    /// The folder's file id (0: a file, or not known).
    frn: u64,
}

/// What reading the folders from the disk found ([`read_refresh`]).
pub struct Refresh {
    generation: u64,
    dirs: Vec<DirDiff>,
}

struct DirDiff {
    node: u32,
    removed: Vec<u32>,
    flags: Vec<(u32, u8)>,
    added: Vec<Added>,
}

struct Added {
    name: String,
    flags: u8,
    frn: u64,
    /// A new folder's contents, walked.
    sub: Option<Index>,
}

/// Step 2 of a refresh: lists the planned folders from the disk and walks new folders. Needs
/// no access to the index (no lock: searches go on meanwhile).
pub fn read_refresh(plan: RefreshPlan, cancel: &AtomicBool) -> Refresh {
    let mut out = Refresh { generation: plan.generation, dirs: Vec::new() };
    for d in plan.dirs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Listing { names, lens, flags, ids } = list(&d.path);
        let mut now: HashMap<String, (u8, u64)> = HashMap::new();
        let mut off = 0usize;
        for ((&len, &f), &frn) in lens.iter().zip(&flags).zip(&ids) {
            now.insert(String::from_utf8_lossy(&names[off..off + len as usize]).into_owned(), (f, frn));
            off += len as usize;
        }
        let mut diff = DirDiff { node: d.node, removed: Vec::new(), flags: Vec::new(), added: Vec::new() };
        for kid in d.kids {
            match now.remove(&kid.name) {
                Some((f, frn)) => {
                    // a folder made again under the same name, or a file that became a folder:
                    // the old entry goes, the new one is read whole
                    let replaced = kid.flags & DIR != 0 && f & DIR != 0 && kid.frn != 0 && frn != 0 && kid.frn != frn;
                    if replaced || (kid.flags & DIR) != (f & DIR) {
                        diff.removed.push(kid.id);
                        now.insert(kid.name, (f, frn));
                    } else {
                        diff.flags.push((kid.id, f));
                    }
                }
                None => diff.removed.push(kid.id),
            }
        }
        for (name, (f, frn)) in now {
            let path = d.path.join(&name);
            let sub = (f & DIR != 0 && f & REPARSE == 0 && !skipped(&path) && !cancel.load(Ordering::Relaxed)).then(|| Index::build(&path, cancel));
            diff.added.push(Added { name, flags: f, frn, sub });
        }
        out.dirs.push(diff);
    }
    out
}

/// [`Index::refresh_dirs`] on a shared index, so that searches are not held up: the index is
/// read to plan, the folders are listed with no lock held, and the result applied in one short
/// write.
pub fn refresh_locked(lock: &parking_lot::RwLock<Option<Index>>, dirs: &[PathBuf], cancel: &AtomicBool) {
    let Some(plan) = lock.read().as_ref().map(|ix| ix.plan_refresh(dirs)) else { return };
    if plan.dirs.is_empty() {
        return;
    }
    let r = read_refresh(plan, cancel);
    if let Some(ix) = lock.write().as_mut() {
        ix.apply_refresh(r);
    }
}

/// [`Index::catch_up`] on a shared index, the same way as [`refresh_locked`].
pub fn catch_up_locked(lock: &parking_lot::RwLock<Option<Index>>, cancel: &AtomicBool) -> Result<usize, crate::usn::Gap> {
    let (root, (id, from)) = {
        let g = lock.read();
        let ix = g.as_ref().ok_or(crate::usn::Gap::Lost)?;
        (ix.root.clone(), ix.journal.ok_or(crate::usn::Gap::Lost)?)
    };
    let (frns, next) = crate::usn::changed_folders(&root, id, from, 200_000)?;
    let plan = {
        let g = lock.read();
        let ix = g.as_ref().ok_or(crate::usn::Gap::Lost)?;
        ix.plan_refresh(&ix.journal_dirs(&frns))
    };
    let count = plan.dirs.len();
    let r = read_refresh(plan, cancel);
    if cancel.load(Ordering::Relaxed) {
        return Ok(count);
    }
    let mut g = lock.write();
    let ix = g.as_mut().ok_or(crate::usn::Gap::Lost)?;
    // (compacted meanwhile: the same records are read again next time)
    if ix.journal.map(|j| j.0) == Some(id) && ix.apply_refresh(r) {
        ix.journal = Some((id, next));
    }
    Ok(count)
}

/// `None` when the folder can't be read this way (the caller falls back).
fn list_with_ids(dir: &Path, push: &mut impl FnMut(&[u16], u32, u64)) -> Option<()> {
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    let w = crate::wide::long_path(dir);
    let h = unsafe { CreateFileW(windows::core::PCWSTR(w.as_ptr()), GENERIC_READ.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None) }.ok()?;
    // u64 elements: the records hold 8-byte fields
    let mut buf = vec![0u64; 8 * 1024];
    let bytes = buf.len() * 8;
    let mut first = true;
    let ok;
    loop {
        let class = if first { FileIdBothDirectoryRestartInfo } else { FileIdBothDirectoryInfo };
        if unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr() as *mut _, bytes as u32) }.is_err() {
            // ERROR_NO_MORE_FILES ends a listing; anything on the first call means "not here"
            ok = !first;
            break;
        }
        first = false;
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let info = unsafe { &*(base.add(off) as *const FILE_ID_BOTH_DIR_INFO) };
            let name = unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2) };
            push(name, info.FileAttributes, info.FileId as u64);
            if info.NextEntryOffset == 0 {
                break;
            }
            off += info.NextEntryOffset as usize;
        }
    }
    unsafe {
        let _ = CloseHandle(h);
    }
    ok.then_some(())
}

struct Build<'a> {
    next: &'a AtomicU32,
    chunks: &'a Mutex<Vec<Chunk>>,
    names: &'a Mutex<Names>,
    cancel: &'a AtomicBool,
}

/// Folders left out of the index: app data, caches, version-control internals and Windows
/// itself. They hold most of a drive's entries (a developer's user folder: 7 million, most of
/// them under AppData, node_modules and build caches) and are rarely searched; a search started
/// inside one walks the disk as before.
const SKIP_NAMES: &[&str] = &["appdata", "node_modules", ".git", "$recycle.bin", "system volume information", "__pycache__", ".cargo", ".rustup", ".gradle", ".nuget"];

/// Folders the user left out (Settings → Search, or a folder's context menu), lowercase.
static USER_SKIP: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

/// Sets the folders the user leaves out of the index.
pub fn set_excluded(dirs: &[PathBuf]) {
    let v = dirs.iter().map(|d| d.to_string_lossy().trim_end_matches('\\').to_lowercase()).collect();
    *USER_SKIP.write().unwrap() = v;
}

/// The built-in exclusions, for showing them.
pub fn default_excluded_names() -> &'static [&'static str] {
    SKIP_NAMES
}

pub fn skipped(dir: &Path) -> bool {
    let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()) else { return false };
    if SKIP_NAMES.contains(&name.as_str()) {
        return true;
    }
    {
        let user = USER_SKIP.read().unwrap();
        if !user.is_empty() {
            let full = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
            if user.iter().any(|u| *u == full) {
                return true;
            }
        }
    }
    if name == "windows" && std::env::var_os("WINDIR").is_some_and(|w| Path::new(&w).eq(dir) || w.to_string_lossy().eq_ignore_ascii_case(&dir.to_string_lossy())) {
        return true;
    }
    // a cache folder says so (Cargo's target/, many tools' caches): https://bford.info/cachedir/
    dir.join("CACHEDIR.TAG").exists()
}

fn walk<'s>(sc: &rayon::Scope<'s>, b: &'s Build<'s>, dir: PathBuf, id: u32) {
    if b.cancel.load(Ordering::Relaxed) {
        return;
    }
    let Listing { names, lens, flags, ids: file_ids } = list(&dir);
    if lens.is_empty() {
        return;
    }
    let frns: Vec<(u32, u64)> = flags.iter().zip(&file_ids).enumerate().filter(|(_, (f, id))| **f & DIR != 0 && **id != 0).map(|(k, (_, id))| (k as u32, *id)).collect();
    let base = b.next.fetch_add(lens.len() as u32, Ordering::Relaxed);
    let ids: Vec<u32> = {
        // one lock per folder
        let mut table = b.names.lock().unwrap();
        let mut off = 0usize;
        lens.iter()
            .map(|&len| {
                let k = table.intern(&names[off..off + len as usize]);
                off += len as usize;
                k
            })
            .collect()
    };
    let mut off = 0usize;
    for (i, (&len, &f)) in lens.iter().zip(&flags).enumerate() {
        let name = &names[off..off + len as usize];
        off += len as usize;
        if f & DIR != 0 && f & REPARSE == 0 {
            let sub = dir.join(String::from_utf8_lossy(name).as_ref());
            let child = base + i as u32;
            // the folder itself is listed (it can be found by name), its contents aren't
            if !skipped(&sub) {
                sc.spawn(move |sc| walk(sc, b, sub, child));
            }
        }
    }
    b.chunks.lock().unwrap().push(Chunk { base, parent: id, ids, flags, frns });
}

fn pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 8))
        .thread_name(|i| format!("rf-index-{i}"))
        .start_handler(|_| unsafe {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        })
        .build()
        .expect("index thread pool")
}

impl Index {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Entries, removed ones included.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.len() <= 1
    }

    /// Live entries (not removed).
    pub fn live(&self) -> usize {
        self.parent.len() - self.removed
    }

    /// Bytes held (arrays, names and lookups).
    pub fn memory(&self) -> usize {
        self.parent.capacity() * 4 + self.name_id.capacity() * 4 + self.flags.capacity() + self.names.memory() + self.dirs.memory() + self.frn.capacity() * 13
    }

    /// Different names held (each stored once).
    pub fn distinct_names(&self) -> usize {
        self.names.len()
    }

    fn name(&self, i: u32) -> &str {
        // names are written from Strings, always valid UTF-8
        std::str::from_utf8(self.names.get(self.name_id[i as usize])).unwrap_or("")
    }

    /// Gives back what the arrays reserved for growing (after a build or a compaction).
    fn shrink(&mut self) {
        self.parent.shrink_to_fit();
        self.name_id.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.names.shrink();
    }

    fn empty(root: PathBuf) -> Index {
        let mut names = Names::new();
        names.intern(b"");
        Index { root, parent: vec![NONE], name_id: vec![0], flags: vec![DIR], names, dirs: IdTable::new(), removed: 0, frn: HashMap::new(), journal: None, generation: 0 }
    }

    /// Walks `root` (blocking; `cancel` stops it, returning what was found so far).
    pub fn build(root: &Path, cancel: &AtomicBool) -> Index {
        // the journal position before walking: whatever changes during the walk is read again
        let journal = crate::usn::query(root).map(|j| (j.id, j.next));
        let next = AtomicU32::new(1);
        let chunks = Mutex::new(Vec::new());
        let mut table = Names::new();
        table.intern(b"");
        let names = Mutex::new(table);
        let b = Build { next: &next, chunks: &chunks, names: &names, cancel };
        pool().scope(|sc| walk(sc, &b, root.to_path_buf(), 0));
        let total = next.load(Ordering::Relaxed) as usize;
        let mut chunks = chunks.into_inner().unwrap();
        chunks.sort_unstable_by_key(|c| c.base);
        let mut ix = Index::empty(root.to_path_buf());
        ix.names = names.into_inner().unwrap();
        ix.parent.resize(total, NONE);
        ix.name_id.resize(total, 0);
        ix.flags.resize(total, REMOVED);
        // each chunk's listing goes as soon as it is laid out
        for c in chunks {
            for (k, (&id, &f)) in c.ids.iter().zip(&c.flags).enumerate() {
                let i = c.base as usize + k;
                ix.parent[i] = c.parent;
                ix.name_id[i] = id;
                ix.flags[i] = f;
            }
            for (k, frn) in c.frns {
                ix.frn.insert(frn, c.base + k);
            }
        }
        if let Some(root_id) = crate::usn::file_id(root) {
            ix.frn.insert(root_id, 0);
        }
        ix.journal = journal;
        // ids reserved by a cancelled walk but never filled stay removed
        ix.removed = ix.flags.iter().filter(|&&f| f & REMOVED != 0).count();
        ix.index_dirs();
        ix.shrink();
        ix
    }

    /// The key a folder is found by: its parent and its folded name.
    fn dir_hash(parent: u32, folded: u64) -> u64 {
        (folded ^ (parent as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)).rotate_left(17)
    }

    fn dir_hash_of(&self, i: u32) -> u64 {
        Self::dir_hash(self.parent[i as usize], fold_key(self.name(i)))
    }

    fn index_dirs(&mut self) {
        self.dirs.clear();
        let dirs: Vec<u32> = (1..self.parent.len() as u32).filter(|&i| self.flags[i as usize] & (DIR | REMOVED) == DIR).collect();
        let hashes: Vec<u64> = dirs.iter().map(|&i| self.dir_hash_of(i)).collect();
        let mut t = IdTable::new();
        t.grow(dirs.len(), &|i| self.dir_hash_of(i));
        // the table is big enough: no rehashing while filling it
        for (&i, &h) in dirs.iter().zip(&hashes) {
            t.insert(h, i, |_| unreachable!("sized for every folder"));
        }
        self.dirs = t;
    }

    /// The folder named `name` (folded `key`) directly in `parent`.
    fn child_dir(&self, parent: u32, key: u64) -> Option<u32> {
        self.dirs.find(Self::dir_hash(parent, key), |i| self.parent[i as usize] == parent && fold_key(self.name(i)) == key)
    }

    /// The node of `path` (a folder at or under the root), if indexed.
    fn node_of(&self, path: &Path) -> Option<u32> {
        let rel = path.strip_prefix(&self.root).ok().or_else(|| {
            // case differences ("c:\Users" against "C:\Users"): the same bytes, folded
            let (r, p) = (self.root.to_str()?, path.to_str()?);
            (p.len() >= r.len() && p.is_char_boundary(r.len()) && p[..r.len()].to_lowercase() == r.to_lowercase()).then(|| Path::new(&p[r.len()..]))
        })?;
        let mut id = 0u32;
        for c in rel.components() {
            let std::path::Component::Normal(n) = c else { continue };
            id = self.child_dir(id, fold_key(&n.to_string_lossy()))?;
        }
        Some(id)
    }

    /// Whether `i` and every folder above it are present (and visible, unless `hidden`); and
    /// whether `under` (if given) is one of them.
    fn valid(&self, mut i: u32, hidden: bool, under: Option<u32>) -> bool {
        let mut inside = under.is_none_or(|u| u == 0);
        // hidden folders count below the search's folder only: searching inside a hidden folder
        // (one the user opened) finds what is in it
        let mut above_under = false;
        while i != 0 && i != NONE {
            let f = self.flags[i as usize];
            if Some(i) == under {
                inside = true;
                above_under = true;
            }
            if f & REMOVED != 0 || (!hidden && !above_under && f & HIDDEN != 0) {
                return false;
            }
            i = self.parent[i as usize];
        }
        inside && i == 0
    }

    /// The full path of node `i`.
    pub fn path(&self, i: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut j = i;
        while j != 0 && j != NONE {
            parts.push(self.name(j));
            j = self.parent[j as usize];
        }
        let mut p = self.root.clone();
        for part in parts.iter().rev() {
            p.push(part);
        }
        p
    }

    /// Entries under `under` (the root when `None`) whose name matches `pattern` (substring, or
    /// a glob with `*` / `?`), at most `max`, in index order. `None` when `under` isn't indexed.
    pub fn search(&self, pattern: &str, under: Option<&Path>, include_hidden: bool, max: usize) -> Option<Vec<PathBuf>> {
        let under = match under {
            Some(p) => {
                // inside a folder the index leaves out: not ours to answer (the disk is walked)
                if p.ancestors().take_while(|a| a.starts_with(&self.root) && *a != self.root.as_path()).any(skipped) {
                    return None;
                }
                Some(self.node_of(p)?)
            }
            None => None,
        };
        let (matched, any) = self.name_matches(pattern);
        if !any {
            return Some(Vec::new());
        }
        let threads = std::thread::available_parallelism().map_or(4, |t| t.get()).clamp(1, 16);
        let matched = &matched;
        // then the entries carrying one of them
        let n = self.parent.len();
        let per = n.div_ceil(threads).max(1);
        let found = Mutex::new(Vec::<(usize, Vec<u32>)>::new());
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (lo, hi) = (t * per, ((t + 1) * per).min(n));
                if lo >= hi {
                    break;
                }
                let (found, stop) = (&found, &stop);
                s.spawn(move || {
                    let mut hits = Vec::new();
                    for i in lo.max(1)..hi {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if self.flags[i] & REMOVED != 0 {
                            continue;
                        }
                        let k = self.name_id[i] as usize;
                        let hit = matched[k / 64] & (1 << (k % 64)) != 0;
                        if hit && self.valid(i as u32, include_hidden, under) {
                            hits.push(i as u32);
                            if hits.len() >= max {
                                break;
                            }
                        }
                    }
                    found.lock().unwrap().push((lo, hits));
                });
            }
        });
        let mut parts = found.into_inner().unwrap();
        parts.sort_unstable_by_key(|p| p.0);
        Some(parts.into_iter().flat_map(|p| p.1).take(max).map(|i| self.path(i)).collect())
    }

    /// Which distinct names match `pattern`: a bit per name (see [`Self::search`]).
    fn name_matches(&self, pattern: &str) -> (Vec<u64>, bool) {
        let m = Matcher::new(pattern);
        let ascii = pattern.trim().to_ascii_lowercase();
        let fast = !ascii.is_empty() && ascii.is_ascii() && !ascii.contains(['*', '?']);
        let needle = ascii.as_bytes();
        let threads = std::thread::available_parallelism().map_or(4, |t| t.get()).clamp(1, 16);
        // each different name once: which ones match (a bit per name)
        let names = self.names.len();
        let mut matched = vec![0u64; names.div_ceil(64)];
        let any = AtomicBool::new(false);
        // whole 64-bit words per thread: each owns its part of the bit set
        let words_per = matched.len().div_ceil(threads).max(1);
        std::thread::scope(|s| {
            for (t, out) in matched.chunks_mut(words_per).enumerate() {
                let (m, any) = (&m, &any);
                s.spawn(move || {
                    let lo = t * words_per * 64;
                    for (w, word) in out.iter_mut().enumerate() {
                        for b in 0..64 {
                            let k = lo + w * 64 + b;
                            if k >= names {
                                break;
                            }
                            let name = self.names.get(k as u32);
                            let hit = if fast && name.is_ascii() {
                                name.len() >= needle.len() && name.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
                            } else {
                                m.is_match(std::str::from_utf8(name).unwrap_or(""))
                            };
                            if hit {
                                *word |= 1 << b;
                            }
                        }
                    }
                    if out.iter().any(|&w| w != 0) {
                        any.store(true, Ordering::Relaxed);
                    }
                });
            }
        });
        let any = any.load(Ordering::Relaxed);
        (matched, any)
    }

    /// Like [`Self::search`], but the best matches first whatever their place in the index:
    /// `rank` scores a matching name (None: not wanted), the best names are kept, and their
    /// entries come back best first (shallower paths first among equals), at most `max`.
    pub fn search_ranked(&self, pattern: &str, under: Option<&Path>, include_hidden: bool, max: usize, rank: &(dyn Fn(&str) -> Option<i32> + Sync)) -> Option<Vec<(i32, PathBuf)>> {
        let under = match under {
            Some(p) => {
                if p.ancestors().take_while(|a| a.starts_with(&self.root) && *a != self.root.as_path()).any(skipped) {
                    return None;
                }
                Some(self.node_of(p)?)
            }
            None => None,
        };
        let (matched, any) = self.name_matches(pattern);
        if !any {
            return Some(Vec::new());
        }
        // score the matching names, keep the best few hundred (a common fragment matches tens
        // of thousands; the best are what is shown)
        const KEEP_NAMES: usize = 600;
        let mut scored: Vec<(i32, u32)> = Vec::new();
        for (w, &word) in matched.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let k = (w * 64 + b) as u32;
                if let Some(sc) = rank(std::str::from_utf8(self.names.get(k)).unwrap_or("")) {
                    scored.push((sc, k));
                }
            }
        }
        if scored.is_empty() {
            return Some(Vec::new());
        }
        scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        scored.truncate(KEEP_NAMES);
        let mut score_of: HashMap<u32, i32> = HashMap::with_capacity(scored.len());
        for (sc, k) in &scored {
            score_of.insert(*k, *sc);
        }
        let threads = std::thread::available_parallelism().map_or(4, |t| t.get()).clamp(1, 16);
        let n = self.parent.len();
        let per = n.div_ceil(threads).max(1);
        let found = Mutex::new(Vec::<(i32, u32, u32)>::new());
        std::thread::scope(|s| {
            for t in 0..threads {
                let (lo, hi) = (t * per, ((t + 1) * per).min(n));
                if lo >= hi {
                    break;
                }
                let (found, score_of) = (&found, &score_of);
                s.spawn(move || {
                    let mut hits = Vec::new();
                    for i in lo.max(1)..hi {
                        if self.flags[i] & REMOVED != 0 {
                            continue;
                        }
                        let Some(&sc) = score_of.get(&self.name_id[i]) else { continue };
                        if self.valid(i as u32, include_hidden, under) {
                            hits.push((sc, self.depth(i as u32), i as u32));
                        }
                    }
                    found.lock().unwrap().extend(hits);
                });
            }
        });
        let mut hits = found.into_inner().unwrap();
        hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        Some(hits.into_iter().take(max).map(|(sc, _, i)| (sc, self.path(i))).collect())
    }

    fn depth(&self, mut i: u32) -> u32 {
        let mut d = 0;
        while i != 0 && i != NONE {
            d += 1;
            i = self.parent[i as usize];
        }
        d
    }

    /// Re-reads `dirs` (folders that changed): entries gone from a folder are marked removed,
    /// new ones appended (new folders walked), the rest keep their place. Folders not in the
    /// index, or inside a folder the index leaves out, are skipped. (The indexer does the same
    /// in three steps so that searches go on meanwhile: [`refresh_locked`].)
    pub fn refresh_dirs(&mut self, dirs: &[PathBuf], cancel: &AtomicBool) {
        let plan = self.plan_refresh(dirs);
        let r = read_refresh(plan, cancel);
        self.apply_refresh(r);
    }

    /// Whether `dir` is a folder the index leaves out, or inside one (below the root).
    fn inside_skipped(&self, dir: &Path) -> bool {
        dir.ancestors().take_while(|a| a.starts_with(&self.root) && *a != self.root.as_path()).any(skipped)
    }

    /// Step 1 of a refresh (needs only to read the index): the folders' nodes and their current
    /// children.
    pub fn plan_refresh(&self, dirs: &[PathBuf]) -> RefreshPlan {
        let mut nodes: Vec<(u32, PathBuf)> = Vec::new();
        for d in dirs {
            // a change directly in a left-out folder (node_modules, .git…) must not bring its
            // contents into the index
            if self.inside_skipped(d) {
                continue;
            }
            if let Some(n) = self.node_of(d) {
                if !nodes.iter().any(|(m, _)| *m == n) {
                    nodes.push((n, d.clone()));
                }
            }
        }
        let mut plan = RefreshPlan { generation: self.generation, dirs: Vec::new() };
        if nodes.is_empty() {
            return plan;
        }
        // their current children, in one pass (a few folders: a short list beats hashing)
        let many = nodes.len() > 16;
        let wanted: HashMap<u32, usize> = if many { nodes.iter().enumerate().map(|(k, (n, _))| (*n, k)).collect() } else { HashMap::new() };
        let mut kids: Vec<Vec<u32>> = vec![Vec::new(); nodes.len()];
        for i in 1..self.parent.len() {
            if self.flags[i] & REMOVED != 0 {
                continue;
            }
            let p = self.parent[i];
            let k = if many { wanted.get(&p).copied() } else { nodes.iter().position(|(n, _)| *n == p) };
            if let Some(k) = k {
                kids[k].push(i as u32);
            }
        }
        // the file ids of the child folders (a folder deleted and made again under the same name
        // has a new one: its contents are read anew)
        let child_dirs: std::collections::HashSet<u32> = kids.iter().flatten().copied().filter(|&i| self.flags[i as usize] & DIR != 0).collect();
        let mut frn_of: HashMap<u32, u64> = HashMap::new();
        if !child_dirs.is_empty() {
            for (&frn, &id) in &self.frn {
                if child_dirs.contains(&id) {
                    frn_of.insert(id, frn);
                }
            }
        }
        for ((node, path), ids) in nodes.into_iter().zip(kids) {
            let kids = ids.into_iter().map(|i| Kid { id: i, name: self.name(i).to_string(), flags: self.flags[i as usize], frn: frn_of.get(&i).copied().unwrap_or(0) }).collect();
            plan.dirs.push(PlanDir { node, path, kids });
        }
        plan
    }

    /// Step 3 of a refresh: applies what [`read_refresh`] found, quickly (no disk access).
    /// False when the index changed shape since the plan (compacted): nothing is applied, the
    /// folders are read again next time.
    pub fn apply_refresh(&mut self, r: Refresh) -> bool {
        if r.generation != self.generation {
            return false;
        }
        for d in r.dirs {
            for k in d.removed {
                self.mark_removed(d.node, k);
            }
            for (k, f) in d.flags {
                self.flags[k as usize] = f;
            }
            for a in d.added {
                let id = self.push(d.node, &a.name, a.flags);
                if a.flags & DIR != 0 && a.frn != 0 {
                    self.frn.insert(a.frn, id);
                }
                if let Some(sub) = a.sub {
                    self.graft(&sub, id);
                }
            }
        }
        true
    }

    fn mark_removed(&mut self, parent: u32, k: u32) {
        if self.flags[k as usize] & REMOVED != 0 {
            return;
        }
        self.flags[k as usize] |= REMOVED;
        self.removed += 1;
        if self.flags[k as usize] & DIR != 0 {
            let h = Self::dir_hash(parent, fold_key(self.name(k)));
            let (parent_of, names, name_id) = (&self.parent, &self.names, &self.name_id);
            let hash_of = |i: u32| Self::dir_hash(parent_of[i as usize], fold_key(std::str::from_utf8(names.get(name_id[i as usize])).unwrap_or("")));
            self.dirs.remove(h, k, hash_of);
        }
    }

    /// The folders (alive, as paths) the change journal names by file id.
    fn journal_dirs(&self, frns: &std::collections::HashSet<u64>) -> Vec<PathBuf> {
        frns.iter().filter_map(|f| self.frn.get(f).copied()).filter(|&n| self.valid(n, true, None)).map(|n| self.path(n)).collect()
    }

    /// Brings a loaded index up to date from the volume's change journal: the folders that
    /// changed since it was saved are listed again. `Err` when the journal can't tell (another
    /// journal, records dropped, too much changed, no journal): walk again then.
    pub fn catch_up(&mut self, cancel: &AtomicBool) -> Result<usize, crate::usn::Gap> {
        let (id, from) = self.journal.ok_or(crate::usn::Gap::Lost)?;
        let (frns, next) = crate::usn::changed_folders(&self.root, id, from, 200_000)?;
        let dirs = self.journal_dirs(&frns);
        let count = dirs.len();
        self.refresh_dirs(&dirs, cancel);
        if !cancel.load(Ordering::Relaxed) {
            self.journal = Some((id, next));
        }
        Ok(count)
    }

    /// The journal position the index is current with (kept up to date by the indexer while it
    /// follows changes).
    pub fn set_journal_next(&mut self, next: i64) {
        if let Some((_, n)) = self.journal.as_mut() {
            *n = next;
        }
    }

    pub fn journal(&self) -> Option<(u64, i64)> {
        self.journal
    }

    fn push(&mut self, parent: u32, name: &str, f: u8) -> u32 {
        let k = self.names.intern(name.as_bytes());
        self.push_id(parent, k, f)
    }

    /// [`Self::push`] with a name already in this index's table.
    fn push_id(&mut self, parent: u32, k: u32, f: u8) -> u32 {
        let id = self.parent.len() as u32;
        self.parent.push(parent);
        self.name_id.push(k);
        self.flags.push(f);
        if f & DIR != 0 {
            let h = self.dir_hash_of(id);
            let (parent_of, names, name_id) = (&self.parent, &self.names, &self.name_id);
            let hash_of = |i: u32| Self::dir_hash(parent_of[i as usize], fold_key(std::str::from_utf8(names.get(name_id[i as usize])).unwrap_or("")));
            self.dirs.insert(h, id, hash_of);
        }
        id
    }

    /// Appends `sub`'s entries (not its root) under node `at`.
    fn graft(&mut self, sub: &Index, at: u32) {
        let base = self.parent.len() as u32 - 1;
        for (&frn, &n) in &sub.frn {
            if n != 0 {
                self.frn.insert(frn, base + n);
            }
        }
        for i in 1..sub.parent.len() as u32 {
            let f = sub.flags[i as usize];
            let p = sub.parent[i as usize];
            let parent = match p {
                0 => at,
                NONE => NONE,
                p => base + p,
            };
            let id = self.push(parent, sub.name(i), f);
            debug_assert_eq!(id, base + i);
            if f & REMOVED != 0 {
                self.removed += 1;
            }
        }
    }

    /// Drops removed entries and everything below them; ids change.
    pub fn compact(&mut self) {
        let n = self.parent.len();
        let mut keep = vec![false; n];
        keep[0] = true;
        // parents come before their children (walk order, appends): one pass decides
        let mut map = vec![NONE; n];
        map[0] = 0;
        // the name table stays (a name no longer used costs a few bytes)
        let names = std::mem::replace(&mut self.names, Names::new());
        let mut out = Index {
            root: self.root.clone(),
            parent: vec![NONE],
            name_id: vec![0],
            flags: vec![DIR],
            names,
            dirs: IdTable::new(),
            removed: 0,
            frn: HashMap::new(),
            journal: self.journal,
            generation: self.generation + 1,
        };
        for i in 1..n {
            let p = self.parent[i];
            if self.flags[i] & REMOVED != 0 || p == NONE || !keep[p as usize] {
                continue;
            }
            keep[i] = true;
            map[i] = out.push_id(map[p as usize], self.name_id[i], self.flags[i]);
        }
        out.frn = self.frn.iter().filter(|(_, n)| map[**n as usize] != NONE).map(|(f, n)| (*f, map[*n as usize])).collect();
        out.shrink();
        *self = out;
    }

    // -------------------------------------------------------------- saving

    /// Version 2: entries (parent, name id, flags), then the name table. Version 1 files
    /// (a name per entry) are read and converted.
    const MAGIC: &'static [u8; 8] = b"RFNIX\0\0\x02";
    const MAGIC_V1: &'static [u8; 8] = b"RFNIX\0\0\x01";

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
            f.write_all(Self::MAGIC)?;
            let root = self.root.to_string_lossy();
            f.write_all(&(root.len() as u32).to_le_bytes())?;
            f.write_all(root.as_bytes())?;
            f.write_all(&(self.parent.len() as u64).to_le_bytes())?;
            f.write_all(&(self.names.len() as u64).to_le_bytes())?;
            f.write_all(&(self.names.pool.len() as u64).to_le_bytes())?;
            f.write_all(as_bytes(&self.parent))?;
            f.write_all(as_bytes(&self.name_id))?;
            f.write_all(&self.flags)?;
            f.write_all(as_bytes(&self.names.off))?;
            f.write_all(&self.names.pool)?;
            // the change journal position and the folders' file ids
            let (jid, jnext) = self.journal.unwrap_or((0, -1));
            f.write_all(&jid.to_le_bytes())?;
            f.write_all(&jnext.to_le_bytes())?;
            f.write_all(&(self.frn.len() as u64).to_le_bytes())?;
            let (mut frns, mut nodes): (Vec<u64>, Vec<u32>) = (Vec::with_capacity(self.frn.len()), Vec::with_capacity(self.frn.len()));
            for (&k, &v) in &self.frn {
                frns.push(k);
                nodes.push(v);
            }
            f.write_all(as_bytes(&frns))?;
            f.write_all(as_bytes(&nodes))?;
            f.flush()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Writes only the change journal position into `path`, saved earlier from this same index
    /// with no change since (an index whose folders didn't change is current with the journal
    /// up to wherever it is now: the changes went elsewhere on the volume). `Ok(false)` when the
    /// file doesn't match this index (then save it whole).
    pub fn save_position(&self, path: &Path) -> std::io::Result<bool> {
        use std::io::{Seek, SeekFrom};
        let Some((jid, jnext)) = self.journal else { return Ok(false) };
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        let rl = read_u32(&mut f)? as u64;
        f.seek(SeekFrom::Current(rl as i64))?;
        let (n, k, pl) = (read_u64(&mut f)?, read_u64(&mut f)?, read_u64(&mut f)?);
        if &magic != Self::MAGIC || n != self.parent.len() as u64 || k != self.names.len() as u64 || pl != self.names.pool.len() as u64 {
            return Ok(false);
        }
        let at = 8 + 4 + rl + 24 + 9 * n + 4 * (k + 1) + pl;
        f.seek(SeekFrom::Start(at))?;
        let saved_id = read_u64(&mut f)?;
        if saved_id != jid && saved_id != 0 {
            return Ok(false);
        }
        f.seek(SeekFrom::Start(at))?;
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&jid.to_le_bytes());
        b[8..].copy_from_slice(&jnext.to_le_bytes());
        f.write_all(&b)?;
        Ok(true)
    }

    /// Reads an index file straight into its arrays (no second copy of the file in memory).
    pub fn load(path: &Path) -> std::io::Result<Index> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not an index file");
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len() as usize;
        let mut f = std::io::BufReader::with_capacity(1 << 20, file);
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        let v1 = &magic == Self::MAGIC_V1;
        if !v1 && &magic != Self::MAGIC {
            return Err(bad());
        }
        let rl = read_u32(&mut f)? as usize;
        if rl > 4096 {
            return Err(bad());
        }
        let mut root = vec![0u8; rl];
        f.read_exact(&mut root)?;
        let root = PathBuf::from(String::from_utf8_lossy(&root).into_owned());
        let n = read_u64(&mut f)? as usize;
        if v1 {
            let nl = read_u64(&mut f)? as usize;
            if n.checked_mul(11).and_then(|x| x.checked_add(nl)).is_none_or(|x| x > size) {
                return Err(bad());
            }
            return Self::load_v1(&mut f, root, n, nl).map_err(|_| bad());
        }
        let k = read_u64(&mut f)? as usize;
        let pl = read_u64(&mut f)? as usize;
        // sizes from the header must fit the file before anything that big is allocated
        if n.checked_mul(9).and_then(|x| x.checked_add(k.checked_add(1)?.checked_mul(4)?)).and_then(|x| x.checked_add(pl)).and_then(|x| x.checked_add(24)).is_none_or(|x| x > size) || n == 0 {
            return Err(bad());
        }
        let parent = read_vec_u32(&mut f, n)?;
        let name_id = read_vec_u32(&mut f, n)?;
        let mut flags = vec![0u8; n];
        f.read_exact(&mut flags)?;
        let off = read_vec_u32(&mut f, k + 1)?;
        let mut pool = vec![0u8; pl];
        f.read_exact(&mut pool)?;
        // every name inside the pool and on a character boundary, every id and parent valid
        if off[0] != 0 || off[k] as usize != pl || off.windows(2).any(|w| w[0] > w[1]) || std::str::from_utf8(&pool).is_err() {
            return Err(bad());
        }
        let text = std::str::from_utf8(&pool).unwrap_or("");
        if off.iter().any(|&o| !text.is_char_boundary(o as usize)) {
            return Err(bad());
        }
        if (0..n).any(|i| name_id[i] as usize >= k || (parent[i] != NONE && parent[i] as usize >= n)) {
            return Err(bad());
        }
        let jid = read_u64(&mut f)?;
        let jnext = read_u64(&mut f)? as i64;
        let nf = read_u64(&mut f)? as usize;
        if nf > n {
            return Err(bad());
        }
        let mut frns = vec![0u64; nf];
        f.read_exact(as_bytes_mut(&mut frns))?;
        let nodes = read_vec_u32(&mut f, nf)?;
        if nodes.iter().any(|&v| v as usize >= n) {
            return Err(bad());
        }
        let removed = flags.iter().filter(|&&f| f & REMOVED != 0).count();
        let mut names = Names { pool, off, lookup: IdTable::new() };
        names.rebuild_lookup();
        let journal = (jnext >= 0).then_some((jid, jnext));
        let mut ix = Index { root, parent, name_id, flags, names, dirs: IdTable::new(), removed, frn: frns.into_iter().zip(nodes).collect(), journal, generation: 0 };
        ix.index_dirs();
        Ok(ix)
    }

    /// A version 1 file: entries with their own name (offset, length) into a pool.
    fn load_v1(f: &mut impl Read, root: PathBuf, n: usize, nl: usize) -> std::io::Result<Index> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not an index file");
        let parent = read_vec_u32(f, n)?;
        let name_off = read_vec_u32(f, n)?;
        let mut name_len = vec![0u16; n];
        f.read_exact(as_bytes_mut(&mut name_len))?;
        let mut flags = vec![0u8; n];
        f.read_exact(&mut flags)?;
        let mut pool = vec![0u8; nl];
        f.read_exact(&mut pool)?;
        let text = std::str::from_utf8(&pool).map_err(|_| bad())?;
        let mut ix = Index::empty(root);
        ix.parent = parent;
        ix.flags = flags;
        ix.name_id = Vec::with_capacity(n);
        for i in 0..n {
            let (o, l) = (name_off[i] as usize, name_len[i] as usize);
            let name = text.get(o..o + l).ok_or_else(bad)?;
            if ix.parent[i] != NONE && ix.parent[i] as usize >= n {
                return Err(bad());
            }
            let id = ix.names.intern(name.as_bytes());
            ix.name_id.push(id);
        }
        ix.removed = ix.flags.iter().filter(|&&f| f & REMOVED != 0).count();
        ix.index_dirs();
        ix.shrink();
        Ok(ix)
    }
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// The arrays as stored: little-endian, which is what Windows' targets are in memory too.
fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    // u8/u16/u32 only: no padding, every bit pattern valid
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn as_bytes_mut<T: Copy>(v: &mut [T]) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, std::mem::size_of_val(v)) }
}

fn read_vec_u32(r: &mut impl Read, n: usize) -> std::io::Result<Vec<u32>> {
    let mut v = vec![0u32; n];
    r.read_exact(as_bytes_mut(&mut v))?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rf-nameindex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, files) in [("", vec!["Rapor.pdf", "notlar.txt"]), ("Proje", vec!["main.rs", "RAPOR eski.docx"]), ("Proje/Çizimler", vec!["ışık.png", "İkon.svg"])] {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            for f in files {
                std::fs::write(d.join(f), b"x").unwrap();
            }
        }
        root
    }

    fn names(v: &[PathBuf]) -> Vec<String> {
        let mut n: Vec<String> = v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        n.sort();
        n
    }

    #[test]
    fn builds_and_finds_case_insensitively() {
        let root = tree("find");
        let ix = Index::build(&root, &AtomicBool::new(false));
        assert_eq!(ix.live(), 9); // the root, 2 folders and 6 files
        assert_eq!(names(&ix.search("rapor", None, false, 100).unwrap()), ["RAPOR eski.docx", "Rapor.pdf"]);
        // Turkish letters fold like the walk search does
        assert_eq!(names(&ix.search("IŞIK", None, false, 100).unwrap()), ["ışık.png"]);
        assert_eq!(names(&ix.search("*.rs", None, false, 100).unwrap()), ["main.rs"]);
        // under a subfolder only
        assert_eq!(names(&ix.search("rapor", Some(&root.join("Proje")), false, 100).unwrap()), ["RAPOR eski.docx"]);
        assert!(ix.search("rapor", Some(&root.join("yok")), false, 100).is_none());
        // the full path comes back
        let p = ix.search("main", None, false, 10).unwrap();
        assert_eq!(p, [root.join("Proje").join("main.rs")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A change inside a folder the index leaves out does not bring its contents in.
    #[test]
    fn left_out_folders_stay_out_after_changes() {
        let root = tree("skipout");
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), b"x").unwrap();
        let mut ix = Index::build(&root, &AtomicBool::new(false));
        std::fs::write(root.join("node_modules/yeni.js"), b"x").unwrap();
        ix.refresh_dirs(&[root.join("node_modules"), root.join("node_modules/pkg")], &AtomicBool::new(false));
        assert!(names(&ix.search("js", None, false, 100).unwrap()).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A folder deleted and made again under the same name: its new contents are read, the old
    /// ones are gone.
    #[test]
    fn a_folder_made_again_is_read_anew() {
        let root = tree("again");
        let mut ix = Index::build(&root, &AtomicBool::new(false));
        if ix.frn.is_empty() {
            eprintln!("no file ids on this volume, skipped");
            std::fs::remove_dir_all(&root).unwrap();
            return;
        }
        std::fs::remove_dir_all(root.join("Proje")).unwrap();
        std::fs::create_dir_all(root.join("Proje")).unwrap();
        std::fs::write(root.join("Proje/yepyeni.txt"), b"x").unwrap();
        // only the root is reported (the folder kept its name)
        ix.refresh_dirs(&[root.clone()], &AtomicBool::new(false));
        assert_eq!(names(&ix.search("yepyeni", None, false, 10).unwrap()), ["yepyeni.txt"]);
        assert!(ix.search("main", None, false, 10).unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A refresh planned before the index was compacted is not applied (ids changed); the
    /// locked refresh lets searches run while the folders are read.
    #[test]
    fn stale_refresh_is_dropped_and_searches_go_on() {
        let root = tree("locked");
        let mut ix = Index::build(&root, &AtomicBool::new(false));
        std::fs::write(root.join("sonradan.txt"), b"x").unwrap();
        let plan = ix.plan_refresh(&[root.clone()]);
        let r = read_refresh(plan, &AtomicBool::new(false));
        ix.compact();
        assert!(!ix.apply_refresh(r));
        assert!(ix.search("sonradan", None, false, 10).unwrap().is_empty());
        let lock = parking_lot::RwLock::new(Some(ix));
        // a reader holds the index while the refresh plans and reads: it is not blocked by it
        let held = lock.read();
        let plan = held.as_ref().unwrap().plan_refresh(&[root.clone()]);
        let r = read_refresh(plan, &AtomicBool::new(false));
        drop(held);
        assert!(lock.write().as_mut().unwrap().apply_refresh(r));
        assert_eq!(names(&lock.read().as_ref().unwrap().search("sonradan", None, false, 10).unwrap()), ["sonradan.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Memory of a saved index, part by part: `RF_INDEX=c.rfidx cargo test -p rf-win --lib --release index_memory -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn index_memory() {
        let p = std::env::var("RF_INDEX").unwrap();
        let t = std::time::Instant::now();
        let ix = Index::load(Path::new(&p)).unwrap();
        let mb = |b: usize| b as f64 / (1 << 20) as f64;
        println!("load {:.0} ms; entries {} (live {}), distinct names {}", t.elapsed().as_secs_f64() * 1000.0, ix.len(), ix.live(), ix.distinct_names());
        println!("parent {:.1} MB, name_id {:.1} MB, flags {:.1} MB", mb(ix.parent.capacity() * 4), mb(ix.name_id.capacity() * 4), mb(ix.flags.capacity()));
        println!("names: pool {:.1} MB, offsets {:.1} MB, lookup {:.1} MB", mb(ix.names.pool.capacity()), mb(ix.names.off.capacity() * 4), mb(ix.names.lookup.memory()));
        println!("dirs table {:.1} MB, frn map {} entries ~{:.1} MB; total {:.1} MB", mb(ix.dirs.memory()), ix.frn.len(), mb(ix.frn.capacity() * 13), mb(ix.memory()));
        for q in ["rapor", "a", "zzzzqx", "*.pdf"] {
            let t = std::time::Instant::now();
            let n = ix.search(q, None, false, 3000).map_or(0, |v| v.len());
            println!("search {q:?}: {n} hits in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// The ranked search returns the best names first, whatever their place in the index, and
    /// shallower paths first among equals.
    #[test]
    fn ranked_search_puts_the_best_first() {
        let root = tree("ranked");
        for i in 0..50 {
            std::fs::write(root.join(format!("eski rapor {i}.txt")), b"x").unwrap();
        }
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/rapor"), b"x").unwrap();
        std::fs::write(root.join("a/rapor"), b"x").unwrap();
        let ix = Index::build(&root, &AtomicBool::new(false));
        let rank = |name: &str| {
            let n = name.to_lowercase();
            if n == "rapor" { Some(1000) } else if n.starts_with("rapor") { Some(800) } else if n.contains("rapor") { Some(400) } else { None }
        };
        let got = ix.search_ranked("rapor", None, false, 3, &rank).unwrap();
        assert_eq!(got[0].1, root.join("a").join("rapor"));
        assert_eq!(got[1].1, root.join("a").join("b").join("rapor"));
        assert_eq!(got[2].0, 800); // Rapor.pdf
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn follows_changes_and_survives_saving() {
        let root = tree("change");
        let mut ix = Index::build(&root, &AtomicBool::new(false));
        std::fs::remove_file(root.join("notlar.txt")).unwrap();
        std::fs::write(root.join("yeni rapor.md"), b"x").unwrap();
        std::fs::create_dir_all(root.join("Arşiv/2026")).unwrap();
        std::fs::write(root.join("Arşiv/2026/eski rapor.pdf"), b"x").unwrap();
        std::fs::remove_dir_all(root.join("Proje")).unwrap();
        ix.refresh_dirs(std::slice::from_ref(&root), &AtomicBool::new(false));
        assert!(ix.search("notlar", None, false, 10).unwrap().is_empty());
        // the removed folder's contents are gone without being touched one by one
        assert!(ix.search("main", None, false, 10).unwrap().is_empty());
        assert_eq!(names(&ix.search("rapor", None, false, 10).unwrap()), ["Rapor.pdf", "eski rapor.pdf", "yeni rapor.md"]);
        let file = root.with_extension("rfidx");
        ix.save(&file).unwrap();
        let mut back = Index::load(&file).unwrap();
        assert_eq!(names(&back.search("rapor", None, false, 10).unwrap()), ["Rapor.pdf", "eski rapor.pdf", "yeni rapor.md"]);
        back.compact();
        assert_eq!(back.removed, 0);
        assert_eq!(names(&back.search("rapor", None, false, 10).unwrap()), ["Rapor.pdf", "eski rapor.pdf", "yeni rapor.md"]);
        assert_eq!(back.search("eski", Some(&root.join("Arşiv")), false, 10).unwrap(), [root.join("Arşiv/2026/eski rapor.pdf")]);
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn caches_are_left_out_but_still_searchable_by_walking() {
        let root = tree("skip");
        std::fs::create_dir_all(root.join("web/node_modules/lodash")).unwrap();
        std::fs::write(root.join("web/node_modules/lodash/rapor.js"), b"x").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        std::fs::write(root.join("build/rapor.o"), b"x").unwrap();
        let ix = Index::build(&root, &AtomicBool::new(false));
        // the folders are found by name, their contents aren't indexed
        assert_eq!(names(&ix.search("node_modules", None, false, 10).unwrap()), ["node_modules"]);
        assert_eq!(names(&ix.search("rapor", None, false, 10).unwrap()), ["RAPOR eski.docx", "Rapor.pdf"]);
        // a search started inside one is left to the disk walk
        assert!(ix.search("rapor", Some(&root.join("web/node_modules")), false, 10).is_none());
        assert!(ix.search("rapor", Some(&root.join("build")), false, 10).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn id_table_finds_inserts_and_removes_like_a_set() {
        // keys collide on purpose (hash = id % 37): long runs, wrap-around, holes refilled
        let hash = |id: u32| (id % 37) as u64 * 0x9e37_79b9;
        let mut t = IdTable::new();
        let mut model = std::collections::HashSet::new();
        let mut x = 12345u32;
        for step in 0..20_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let id = (x >> 8) % 500;
            if model.contains(&id) && step % 3 != 0 {
                t.remove(hash(id), id, hash);
                model.remove(&id);
            } else if !model.contains(&id) {
                t.insert(hash(id), id, hash);
                model.insert(id);
            }
            if step % 997 == 0 {
                for probe in 0..500 {
                    assert_eq!(t.find(hash(probe), |i| i == probe).is_some(), model.contains(&probe), "step {step}, id {probe}");
                }
            }
        }
        assert_eq!(t.len, model.len());
    }

    #[test]
    fn names_are_stored_once_and_old_files_convert() {
        let root = tree("names");
        for d in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("README.md"), b"x").unwrap();
        }
        let ix = Index::build(&root, &AtomicBool::new(false));
        assert_eq!(names(&ix.search("readme", None, false, 10).unwrap()), ["README.md"; 3]);
        // "", 3 folder names, README.md and the tree's 8 names: 13, not 16
        assert_eq!(ix.distinct_names(), 13);
        // a version 1 file (a name per entry) reads into the same index
        let file = root.with_extension("v1.rfidx");
        {
            let mut f = std::fs::File::create(&file).unwrap();
            let r = ix.root.to_string_lossy();
            let n = ix.parent.len();
            let mut pool = Vec::new();
            let (mut offs, mut lens) = (Vec::new(), Vec::new());
            for i in 0..n as u32 {
                offs.push(pool.len() as u32);
                lens.push(ix.name(i).len() as u16);
                pool.extend_from_slice(ix.name(i).as_bytes());
            }
            f.write_all(Index::MAGIC_V1).unwrap();
            f.write_all(&(r.len() as u32).to_le_bytes()).unwrap();
            f.write_all(r.as_bytes()).unwrap();
            f.write_all(&(n as u64).to_le_bytes()).unwrap();
            f.write_all(&(pool.len() as u64).to_le_bytes()).unwrap();
            f.write_all(as_bytes(&ix.parent)).unwrap();
            f.write_all(as_bytes(&offs)).unwrap();
            f.write_all(as_bytes(&lens)).unwrap();
            f.write_all(&ix.flags).unwrap();
            f.write_all(&pool).unwrap();
        }
        let back = Index::load(&file).unwrap();
        assert_eq!(back.distinct_names(), 13);
        assert_eq!(back.search("readme", Some(&root.join("b")), false, 10).unwrap(), [root.join("b").join("README.md")]);
        // a truncated file is refused, not trusted
        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 3]).unwrap();
        assert!(Index::load(&file).is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn catches_up_from_the_change_journal() {
        let root = tree("journal");
        if crate::usn::query(&root).is_none() {
            eprintln!("no change journal here, skipped");
            return;
        }
        let ix = Index::build(&root, &AtomicBool::new(false));
        assert!(ix.journal().is_some());
        let file = root.with_extension("j.rfidx");
        ix.save(&file).unwrap();
        drop(ix);
        // while "closed": a file deep inside, a new folder with content, a deletion, a rename
        std::fs::write(root.join("Proje/Çizimler/yeni rapor.png"), b"x").unwrap();
        std::fs::create_dir_all(root.join("Yeni/İç")).unwrap();
        std::fs::write(root.join("Yeni/İç/rapor özet.txt"), b"x").unwrap();
        std::fs::remove_file(root.join("Rapor.pdf")).unwrap();
        std::fs::rename(root.join("Proje"), root.join("Projeler")).unwrap();
        let mut back = Index::load(&file).unwrap();
        let changed = back.catch_up(&AtomicBool::new(false)).unwrap();
        assert!(changed >= 1);
        let found = back.search("rapor", None, false, 20).unwrap();
        assert_eq!(names(&found), ["RAPOR eski.docx", "rapor özet.txt", "yeni rapor.png"]);
        assert!(found.contains(&root.join("Projeler/Çizimler/yeni rapor.png")), "{found:?}");
        // caught up: nothing more to read
        assert_eq!(back.catch_up(&AtomicBool::new(false)).unwrap(), 0);
        // the position alone is written in place, the rest of the file untouched
        back.save(&file).unwrap();
        let (jid, next) = back.journal().unwrap();
        back.set_journal_next(next + 1);
        assert!(back.save_position(&file).unwrap());
        assert_eq!(Index::load(&file).unwrap().journal(), Some((jid, next + 1)));
        // a file of another shape is refused
        back.push(0, "başka", 0);
        assert!(!back.save_position(&file).unwrap());
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hidden_entries_need_asking() {
        let root = tree("hidden");
        let secret = root.join("gizli");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("rapor gizli.txt"), b"x").unwrap();
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = secret.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            let _ = windows::Win32::Storage::FileSystem::SetFileAttributesW(windows::core::PCWSTR(w.as_ptr()), windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN);
        }
        let ix = Index::build(&root, &AtomicBool::new(false));
        assert!(!names(&ix.search("gizli", None, false, 10).unwrap()).contains(&"rapor gizli.txt".to_string()));
        assert!(names(&ix.search("gizli", None, true, 10).unwrap()).contains(&"rapor gizli.txt".to_string()));
        // searching inside the hidden folder the user opened finds what is in it
        assert_eq!(names(&ix.search("rapor", Some(&secret), false, 10).unwrap()), ["rapor gizli.txt"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

impl Index {
    /// The `n` folders holding the most entries below them (whole subtrees), largest first —
    /// for deciding what is worth indexing.
    pub fn heaviest(&self, n: usize, depth_limit: usize) -> Vec<(PathBuf, usize)> {
        let len = self.parent.len();
        let mut count = vec![0usize; len];
        // children come after their parents: one backwards pass sums subtrees
        for i in (1..len).rev() {
            if self.flags[i] & REMOVED == 0 {
                let p = self.parent[i];
                if p != NONE {
                    count[p as usize] += count[i] + 1;
                }
            }
        }
        let depth = |mut i: u32| {
            let mut d = 0;
            while i != 0 && i != NONE {
                d += 1;
                i = self.parent[i as usize];
            }
            d
        };
        let mut v: Vec<(u32, usize)> = (1..len as u32).filter(|&i| self.flags[i as usize] & DIR != 0 && depth(i) <= depth_limit).map(|i| (i, count[i as usize])).collect();
        v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        v.into_iter().take(n).map(|(i, c)| (self.path(i), c)).collect()
    }
}
