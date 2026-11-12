//! Our own parallel permanent delete (Shift+Delete). The shell engine removes one file at a
//! time with a lot of bookkeeping per item; deleting the files of a tree in parallel and then
//! the emptied folders (deepest first) is much faster on SSDs.
//!
//! Only for permanent deletes: the recycle bin stays with the shell engine. Links and
//! junctions are removed as links (their targets are never entered). Whatever fails here
//! (in use, access denied, …) is handed to the shell engine at the end, which can retry or
//! ask for elevation.
//!
//! Blocking: call from a worker thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::fileops::Tracker;

fn is_reparse(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// A link's own directory bit: std reports junctions and folder symlinks as `is_dir() == false`
/// (they are "symlinks"), but they must be removed with RemoveDirectory.
fn dir_attr(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY.0 != 0
}

/// Removes a file, clearing the read-only attribute if that is what stops it.
fn remove_file(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let mut perm = std::fs::symlink_metadata(p)?.permissions();
            if !perm.readonly() {
                return Err(e);
            }
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(p, perm)?;
            std::fs::remove_file(p)
        }
        r => r,
    }
}

/// A folder entry: links / junctions to folders are removed with `remove_dir` (the link only).
fn remove_link_or_dir(p: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(p).or_else(|e| {
        // a read-only folder can't be removed either
        let mut perm = std::fs::symlink_metadata(p)?.permissions();
        if !perm.readonly() {
            return Err(e);
        }
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(p, perm)?;
        std::fs::remove_dir(p)
    })
}

/// Walks `root` without following links: files (including links to files), folder links,
/// and real folders in walk order (parents before children).
fn walk(root: &Path, files: &mut Vec<PathBuf>, links: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let m = e.metadata()?;
            let p = e.path();
            if is_reparse(&m) {
                if dir_attr(&m) { links.push(p) } else { files.push(p) }
            } else if m.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
        dirs.push(d);
    }
    Ok(())
}

/// Permanently deletes `paths`. Returns like `fileops::delete_tracked`.
pub fn delete_permanent(paths: &[PathBuf], tracker: Arc<Tracker>, threads: usize) -> anyhow::Result<bool> {
    delete_inner(paths, tracker, threads, true)
}

/// Removes the source of a finished move: exactly the `files` that were verified at the
/// destination, then `dirs` (parents before children in the list) deepest first, and only when
/// empty. Something added to the source meanwhile is never touched; its folder then stays.
/// No shell fallback. `true` when everything listed is gone.
pub fn remove_verified(files: &[PathBuf], dirs: &[PathBuf]) -> bool {
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let threads = threads_for(dirs.first().map(std::slice::from_ref).unwrap_or(files)).min(files.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while let Some(f) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if let Err(e) = remove_file(f) {
                        log::info!("move: source {} kept ({e})", f.display());
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    for d in dirs.iter().rev() {
        // remove_dir refuses a folder that isn't empty: whatever appeared there stays
        if remove_link_or_dir(d).is_err() {
            failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    failed.load(Ordering::Relaxed) == 0
}

fn delete_inner(paths: &[PathBuf], tracker: Arc<Tracker>, threads: usize, shell_fallback: bool) -> anyhow::Result<bool> {
    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut to_shell = Vec::new();
    for p in paths {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            to_shell.push(p.clone());
            continue;
        };
        if is_reparse(&m) {
            if dir_attr(&m) { links.push(p.clone()) } else { files.push(p.clone()) }
        } else if m.is_dir() {
            let (mut f, mut l, mut d) = (Vec::new(), Vec::new(), Vec::new());
            if walk(p, &mut f, &mut l, &mut d).is_err() {
                // can't list it all (access denied somewhere): the shell handles the whole item
                to_shell.push(p.clone());
                continue;
            }
            files.extend(f);
            links.extend(l);
            dirs.extend(d);
        } else {
            files.push(p.clone());
        }
    }
    let total = (files.len() + links.len() + dirs.len()).max(1);
    let done = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    let report = |name: Option<String>| {
        let d = done.load(Ordering::Relaxed).min(total);
        (tracker.on_progress)(1000, (d * 1000 / total) as u32, name);
    };
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, files.len().max(1)) {
            s.spawn(|| {
                while !tracker.cancel.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(f) = files.get(i) else { return };
                    if let Err(e) = remove_file(f) {
                        log::info!("fastdelete: {} ({e})", f.display());
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed);
                    if n % 128 == 0 {
                        report(f.file_name().map(|n| n.to_string_lossy().into_owned()));
                    }
                }
            });
        }
    });
    if tracker.cancel.load(Ordering::Relaxed) {
        return Ok(false);
    }
    for l in &links {
        if let Err(e) = remove_link_or_dir(l) {
            log::info!("fastdelete: link {} ({e})", l.display());
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    // deepest first: walk order puts parents before their children
    for d in dirs.iter().rev() {
        if let Err(e) = remove_link_or_dir(d) {
            log::info!("fastdelete: dir {} ({e})", d.display());
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    report(None);
    // whatever is still there goes to the shell (it shows what is wrong, offers elevation)
    let left: Vec<PathBuf> = paths.iter().filter(|p| std::fs::symlink_metadata(p).is_ok() && !to_shell.contains(p)).cloned().collect();
    to_shell.extend(left);
    let mut ok = true;
    if !shell_fallback {
        return Ok(to_shell.is_empty());
    }
    #[cfg(test)]
    if !to_shell.is_empty() {
        panic!("would hand to the shell: {to_shell:?}");
    }
    if !to_shell.is_empty() {
        ok = crate::fileops::delete_tracked(&to_shell, true, tracker)?;
    }
    let mut parents: Vec<&Path> = paths.iter().filter_map(|p| p.parent()).collect();
    parents.dedup();
    for p in parents {
        crate::shell::notify_updated(p);
    }
    Ok(ok)
}

/// Thread count for deleting under `paths`: SSDs like many at once, spinning disks don't.
pub fn threads_for(paths: &[PathBuf]) -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    match paths.first().and_then(|p| crate::drives::seek_penalty(p)) {
        Some(true) => 2,
        Some(false) => cores.clamp(2, 16),
        None => cores.clamp(2, 8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn tracker() -> Arc<Tracker> {
        Arc::new(Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false))))
    }

    #[test]
    fn deletes_tree_but_not_link_targets() {
        let base = std::env::temp_dir().join(format!("rf-fastdel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let victim = base.join("silinecek");
        let keep = base.join("korunacak");
        std::fs::create_dir_all(victim.join("a/b/c")).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("önemli.txt"), b"dokunma").unwrap();
        for i in 0..500 {
            std::fs::write(victim.join(format!("a/b/c/f{i}.txt")), b"x").unwrap();
        }
        let ro = victim.join("a/salt-okunur.txt");
        std::fs::write(&ro, b"ro").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
        // a junction inside the victim pointing at the folder we must keep (no admin needed)
        let j = victim.join("bağlantı");
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&j)
            .arg(&keep)
            .output()
            .unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        let lone = base.join("tek.txt");
        std::fs::write(&lone, b"1").unwrap();
        assert!(delete_permanent(&[victim.clone(), lone.clone()], tracker(), 8).unwrap());
        assert!(!victim.exists() && !lone.exists());
        assert_eq!(std::fs::read(keep.join("önemli.txt")).unwrap(), b"dokunma");
        let _ = std::fs::remove_dir_all(&base);
    }
}
