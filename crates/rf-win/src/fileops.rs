//! File operations through the Windows shell copy engine (IFileOperation): same behavior as
//! Explorer (recycle bin, undo record, conflict dialogs, elevation prompts when needed).
//! Blocking — call from a worker thread.

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Shell::{
    FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOF_RENAMEONCOLLISION, FOFX_ADDUNDORECORD,
    FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
};
use windows::core::PCWSTR;

use crate::shell::ensure_com;
use crate::wide::wide;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Copy,
    Move,
}

fn item(p: &Path) -> anyhow::Result<IShellItem> {
    let w = wide(p);
    Ok(unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)? })
}

fn new_op(flags: u32) -> anyhow::Result<IFileOperation> {
    ensure_com();
    let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL)? };
    unsafe { op.SetOperationFlags(FILEOPERATION_FLAGS(flags))? };
    Ok(op)
}

/// Returns Ok(true) when everything completed, Ok(false) when the user cancelled part of it.
fn perform(op: &IFileOperation) -> anyhow::Result<bool> {
    unsafe {
        op.PerformOperations()?;
        Ok(!op.GetAnyOperationsAborted()?.as_bool())
    }
}

/// Copies `sources` into `dest_dir` with the shell's engine and the given FOF_* flags (no undo
/// record unless the flags ask for one).
pub fn shell_copy(sources: &[PathBuf], dest_dir: &Path, flags: u32) -> anyhow::Result<()> {
    let op = new_op(flags)?;
    let dest = item(dest_dir)?;
    for s in sources {
        let it = item(s)?;
        unsafe { op.CopyItem(&it, &dest, PCWSTR::null(), None)? };
    }
    anyhow::ensure!(perform(&op)?, "the copy was aborted");
    Ok(())
}

/// Copy or move into `dest_dir`. Pasting into the same folder makes "x - Kopya" names.
pub fn transfer(kind: Transfer, sources: &[PathBuf], dest_dir: &Path) -> anyhow::Result<bool> {
    let same_dir = sources.iter().all(|s| s.parent() == Some(dest_dir));
    if kind == Transfer::Move && same_dir {
        return Ok(true); // moving onto itself: no-op
    }
    let mut flags = FOF_ALLOWUNDO.0 as u32 | FOF_NOCONFIRMMKDIR.0 as u32 | FOFX_ADDUNDORECORD.0;
    if same_dir {
        flags |= FOF_RENAMEONCOLLISION.0 as u32;
    }
    let op = new_op(flags)?;
    let dest = item(dest_dir)?;
    for s in sources {
        let it = item(s)?;
        unsafe {
            match kind {
                Transfer::Copy => op.CopyItem(&it, &dest, PCWSTR::null(), None)?,
                Transfer::Move => op.MoveItem(&it, &dest, PCWSTR::null(), None)?,
            }
        }
    }
    perform(&op)
}

/// Delete to the recycle bin (`permanent = false`) or permanently (Windows asks for confirmation).
pub fn delete(paths: &[PathBuf], permanent: bool) -> anyhow::Result<bool> {
    let flags = if permanent { 0 } else { FOF_ALLOWUNDO.0 as u32 | FOFX_RECYCLEONDELETE.0 | FOFX_ADDUNDORECORD.0 };
    let op = new_op(flags)?;
    for p in paths {
        unsafe { op.DeleteItem(&item(p)?, None)? };
    }
    perform(&op)
}

pub fn rename(path: &Path, new_name: &str) -> anyhow::Result<bool> {
    let op = new_op(FOF_ALLOWUNDO.0 as u32 | FOFX_ADDUNDORECORD.0)?;
    let w = wide(new_name);
    unsafe { op.RenameItem(&item(path)?, PCWSTR(w.as_ptr()), None)? };
    perform(&op)
}

/// Plain rename that fails instead of silently replacing an existing target
/// (`std::fs::rename` replaces on Windows).
pub fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    let (a, b) = (crate::wide::long_path(from), crate::wide::long_path(to));
    unsafe { MoveFileExW(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr()), MOVE_FILE_FLAGS(0)) }.map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

/// "Yeni klasör", "Yeni klasör (2)", … — returns the created path.
pub fn new_folder(dir: &Path) -> anyhow::Result<PathBuf> {
    let p = unique_name(dir, "Yeni klasör", "");
    std::fs::create_dir(&p)?;
    Ok(p)
}

pub fn new_text_file(dir: &Path) -> anyhow::Result<PathBuf> {
    let p = unique_name(dir, "Yeni Metin Belgesi", ".txt");
    std::fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
    Ok(p)
}

pub fn unique_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}


// ---------------------------------------------------------------------------------------------
// Tracked operations (in-app Status Center): progress callback + cancellation, no shell
// progress window (conflict / elevation dialogs still come from Windows).
// ---------------------------------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::E_ABORT;
use windows::Win32::UI::Shell::{FOF_SILENT, IFileOperationProgressSink, IFileOperationProgressSink_Impl, SIGDN_NORMALDISPLAY};
use windows_core::{HRESULT, Ref, implement};

/// Progress reporting for one operation. `on_progress(total, done, current_item)`;
/// `total`/`done` are the shell's abstract work units (use done/total as a fraction).
pub struct Tracker {
    pub on_progress: Box<dyn Fn(u32, u32, Option<String>) + Send + Sync>,
    pub cancel: Arc<AtomicBool>,
    /// (source, result) for every item the shell finished: the new copy/moved location, or the
    /// item's path inside the Recycle Bin for deletes. Used to build undo records.
    pub results: std::sync::Mutex<Vec<(PathBuf, PathBuf)>>,
    /// Our engine waits while this is set (the shell engine can't pause).
    pub paused: Arc<AtomicBool>,
    /// Copy speed limit in bytes per second; 0 = none (our engine only).
    pub limit: std::sync::atomic::AtomicU64,
    /// Check copies after writing them (our engine only); see [`Tracker::set_verify`].
    pub verify: std::sync::atomic::AtomicU8,
    /// Asked (on the worker thread, blocking) what to do with names that already exist at the
    /// destination; `None` cancels the operation. Without it, both are kept.
    #[allow(clippy::type_complexity)]
    pub ask: Option<Box<dyn Fn(&[crate::fastcopy::Conflict]) -> Option<Vec<crate::fastcopy::Resolve>> + Send + Sync>>,
}

impl Tracker {
    pub fn set_verify(&self, v: crate::fastcopy::Verify) {
        self.verify.store(v as u8, Ordering::Relaxed);
    }

    pub fn verify_mode(&self) -> crate::fastcopy::Verify {
        match self.verify.load(Ordering::Relaxed) {
            1 => crate::fastcopy::Verify::Quick,
            2 => crate::fastcopy::Verify::Full,
            _ => crate::fastcopy::Verify::Off,
        }
    }

    pub fn new(on_progress: Box<dyn Fn(u32, u32, Option<String>) + Send + Sync>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            on_progress,
            cancel,
            results: Default::default(),
            paused: Default::default(),
            limit: Default::default(),
            verify: Default::default(),
            ask: None,
        }
    }
}

fn fs_path(item: Ref<IShellItem>) -> Option<PathBuf> {
    let it = item.as_ref()?;
    unsafe {
        let p = it.GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

#[implement(IFileOperationProgressSink)]
struct Sink {
    tracker: Arc<Tracker>,
    last: std::sync::Mutex<(u32, u32)>,
}

impl Sink {
    fn check(&self) -> windows_core::Result<()> {
        if self.tracker.cancel.load(Ordering::Relaxed) { Err(E_ABORT.into()) } else { Ok(()) }
    }
    fn record(&self, hr: HRESULT, src: Ref<IShellItem>, created: Ref<IShellItem>) {
        if hr.is_ok() {
            if let (Some(a), Some(b)) = (fs_path(src), fs_path(created)) {
                self.tracker.results.lock().unwrap().push((a, b));
            }
        }
    }
    fn item(&self, item: Ref<IShellItem>) {
        if let Some(it) = item.as_ref() {
            if let Ok(name) = unsafe { it.GetDisplayName(SIGDN_NORMALDISPLAY) } {
                let s = unsafe { name.to_string().unwrap_or_default() };
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const _)) };
                let (t, d) = *self.last.lock().unwrap();
                (self.tracker.on_progress)(t, d, Some(s));
            }
        }
    }
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> windows_core::Result<()> { Ok(()) }
    fn FinishOperations(&self, _hr: HRESULT) -> windows_core::Result<()> { Ok(()) }
    fn PreRenameItem(&self, _f: u32, item: Ref<IShellItem>, _n: &PCWSTR) -> windows_core::Result<()> { self.item(item); self.check() }
    fn PostRenameItem(&self, _f: u32, _i: Ref<IShellItem>, _n: &PCWSTR, _hr: HRESULT, _c: Ref<IShellItem>) -> windows_core::Result<()> { self.check() }
    fn PreMoveItem(&self, _f: u32, item: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows_core::Result<()> { self.item(item); self.check() }
    fn PostMoveItem(&self, _f: u32, i: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR, hr: HRESULT, c: Ref<IShellItem>) -> windows_core::Result<()> { self.record(hr, i, c); self.check() }
    fn PreCopyItem(&self, _f: u32, item: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows_core::Result<()> { self.item(item); self.check() }
    fn PostCopyItem(&self, _f: u32, i: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR, hr: HRESULT, c: Ref<IShellItem>) -> windows_core::Result<()> { self.record(hr, i, c); self.check() }
    fn PreDeleteItem(&self, _f: u32, item: Ref<IShellItem>) -> windows_core::Result<()> { self.item(item); self.check() }
    fn PostDeleteItem(&self, _f: u32, i: Ref<IShellItem>, hr: HRESULT, c: Ref<IShellItem>) -> windows_core::Result<()> { self.record(hr, i, c); self.check() }
    fn PreNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows_core::Result<()> { self.check() }
    fn PostNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR, _t: &PCWSTR, _a: u32, _hr: HRESULT, _c: Ref<IShellItem>) -> windows_core::Result<()> { self.check() }
    fn UpdateProgress(&self, total: u32, done: u32) -> windows_core::Result<()> {
        *self.last.lock().unwrap() = (total, done);
        (self.tracker.on_progress)(total, done, None);
        self.check()
    }
    fn ResetTimer(&self) -> windows_core::Result<()> { Ok(()) }
    fn PauseTimer(&self) -> windows_core::Result<()> { Ok(()) }
    fn ResumeTimer(&self) -> windows_core::Result<()> { Ok(()) }
}

fn perform_tracked(op: &IFileOperation, tracker: Arc<Tracker>) -> anyhow::Result<bool> {
    let sink: IFileOperationProgressSink = Sink { tracker: tracker.clone(), last: std::sync::Mutex::new((0, 0)) }.into();
    let cookie = unsafe { op.Advise(&sink)? };
    let r = perform(op);
    unsafe {
        let _ = op.Unadvise(cookie);
    }
    if tracker.cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }
    r
}

/// `transfer` with in-app progress (no shell progress window).
pub fn transfer_tracked(kind: Transfer, sources: &[PathBuf], dest_dir: &Path, tracker: Arc<Tracker>) -> anyhow::Result<bool> {
    let same_dir = sources.iter().all(|s| s.parent() == Some(dest_dir));
    if kind == Transfer::Move && same_dir {
        return Ok(true);
    }
    let mut flags = FOF_ALLOWUNDO.0 as u32 | FOF_NOCONFIRMMKDIR.0 as u32 | FOFX_ADDUNDORECORD.0 | FOF_SILENT.0 as u32;
    if same_dir {
        flags |= FOF_RENAMEONCOLLISION.0 as u32;
    }
    let op = new_op(flags)?;
    let dest = item(dest_dir)?;
    for s in sources {
        let it = item(s)?;
        unsafe {
            match kind {
                Transfer::Copy => op.CopyItem(&it, &dest, PCWSTR::null(), None)?,
                Transfer::Move => op.MoveItem(&it, &dest, PCWSTR::null(), None)?,
            }
        }
    }
    perform_tracked(&op, tracker)
}

/// `delete` with in-app progress.
pub fn delete_tracked(paths: &[PathBuf], permanent: bool, tracker: Arc<Tracker>) -> anyhow::Result<bool> {
    let mut flags = if permanent { 0 } else { FOF_ALLOWUNDO.0 as u32 | FOFX_RECYCLEONDELETE.0 | FOFX_ADDUNDORECORD.0 };
    flags |= FOF_SILENT.0 as u32;
    let op = new_op(flags)?;
    for p in paths {
        unsafe { op.DeleteItem(&item(p)?, None)? };
    }
    perform_tracked(&op, tracker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique() {
        let d = std::env::temp_dir().join(format!("rf-uniq-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = new_folder(&d).unwrap();
        let b = new_folder(&d).unwrap();
        assert!(a.ends_with("Yeni klasör"));
        assert!(b.ends_with("Yeni klasör (2)"));
        let f = new_text_file(&d).unwrap();
        assert!(rename(&f, "x.txt").unwrap());
        assert!(d.join("x.txt").exists());
        assert!(transfer(Transfer::Copy, &[d.join("x.txt")], &d).unwrap());
        let n = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(n, 4); // 2 folders, x.txt, x - Kopya.txt
        // tracked copy reports progress
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let t = Arc::new(Tracker::new(
            Box::new(move |_, _, _| {
                c2.fetch_add(1, Ordering::Relaxed);
            }),
            Arc::new(AtomicBool::new(false)),
        ));
        std::fs::create_dir_all(d.join("dst")).unwrap();
        assert!(transfer_tracked(Transfer::Copy, &[d.join("x.txt")], &d.join("dst"), t.clone()).unwrap());
        assert!(d.join("dst").join("x.txt").exists());
        assert!(calls.load(Ordering::Relaxed) > 0);
        let res = t.results.lock().unwrap().clone();
        assert_eq!(res.len(), 1);
        assert!(res[0].1.ends_with("dst\\x.txt"), "{res:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
