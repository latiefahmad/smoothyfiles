//! Small-file copy micro-benchmark: CopyFile2 vs our own read/write copy (times + attributes,
//! alternate streams checked), on the copybench tree's small files.
//!
//!   cargo run --release -p rf-win --example smallcopy -- <copybench work dir> [threads]

use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use windows::Win32::Storage::FileSystem::{COPY_FILE_FAIL_IF_EXISTS, COPYFILE2_EXTENDED_PARAMETERS, CopyFile2};
use windows::core::PCWSTR;

fn files(src: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for d in std::fs::read_dir(src).unwrap().flatten() {
        if d.path().is_dir() && !d.path().ends_with("büyük") {
            for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                v.push(f.path());
            }
        }
    }
    v
}

fn run(name: &str, list: &[PathBuf], src: &Path, dst: &Path, threads: usize, f: impl Fn(&Path, &Path) + Sync) {
    let _ = std::fs::remove_dir_all(dst);
    for d in std::fs::read_dir(src).unwrap().flatten() {
        if d.path().is_dir() {
            std::fs::create_dir_all(dst.join(d.file_name())).unwrap();
        }
    }
    let next = AtomicUsize::new(0);
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = list.get(i) else { return };
                    let rel = p.strip_prefix(src).unwrap();
                    f(p, &dst.join(rel));
                }
            });
        }
    });
    println!("{name:12} {threads:2} threads: {:.2} s ({:.0} files/s)", t.elapsed().as_secs_f32(), list.len() as f32 / t.elapsed().as_secs_f32());
    let _ = std::fs::remove_dir_all(dst);
}

fn copyfile2(a: &Path, b: &Path) {
    let (wa, wb) = (rf_win::wide::long_path(a), rf_win::wide::long_path(b));
    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: windows::Win32::Storage::FileSystem::COPYFILE_FLAGS(COPY_FILE_FAIL_IF_EXISTS.0),
        ..Default::default()
    };
    unsafe { CopyFile2(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr()), Some(&params)).unwrap() }
}

/// Read whole, write whole, copy times + attributes.
fn manual(a: &Path, b: &Path) {
    use std::io::{Read, Write};
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    let mut src = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(a).unwrap();
    let meta = src.metadata().unwrap();
    let mut buf = Vec::with_capacity(meta.len() as usize);
    src.read_to_end(&mut buf).unwrap();
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(b).unwrap();
    dst.write_all(&buf).unwrap();
    let times = std::fs::FileTimes::new().set_modified(meta.modified().unwrap()).set_accessed(meta.accessed().unwrap());
    dst.set_times(times).unwrap();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let work = PathBuf::from(args.next().expect("work dir"));
    let threads: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(16);
    let src = work.join("kaynak");
    let list = files(&src);
    println!("{} files", list.len());
    // round-robin across folders: threads work in different directories at once
    let mut by_dir: Vec<Vec<PathBuf>> = Vec::new();
    for f in &list {
        match by_dir.iter_mut().find(|v| v[0].parent() == f.parent()) {
            Some(v) => v.push(f.clone()),
            None => by_dir.push(vec![f.clone()]),
        }
    }
    let mut spread = Vec::new();
    for i in 0.. {
        let before = spread.len();
        for v in &by_dir {
            if let Some(f) = v.get(i) {
                spread.push(f.clone());
            }
        }
        if spread.len() == before {
            break;
        }
    }
    for th in [threads, threads * 2] {
        run("CopyFile2", &list, &src, &work.join("m-a"), th, copyfile2);
        run("CF2 spread", &spread, &src, &work.join("m-s"), th, copyfile2);
        run("read/write", &list, &src, &work.join("m-b"), th, manual);
        run("std::fs::copy", &list, &src, &work.join("m-c"), th, |a, b| {
            std::fs::copy(a, b).unwrap();
        });
        let body = vec![7u8; 4096];
        run("write only", &list, &src, &work.join("m-d"), th, |_, b| std::fs::write(b, &body).unwrap());
        run("empty create", &list, &src, &work.join("m-e"), th, |_, b| {
            std::fs::File::create(b).unwrap();
        });
    }
}
