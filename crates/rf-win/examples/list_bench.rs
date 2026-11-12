//! Directory listing speed, single thread, whole tree: FindFirstFileExW (large fetch) vs
//! GetFileInformationByHandleEx(FileFullDirectoryInfo) with big buffers.
//! `cargo run --release -p rf-win --example list_bench -- <dir>`
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch,
    FindFirstFileExW, FindNextFileW, FIND_FIRST_EX_LARGE_FETCH, GetFileInformationByHandleEx, OPEN_EXISTING, WIN32_FIND_DATAW,
};
use windows::core::PCWSTR;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

#[derive(Default, Debug)]
struct T {
    dirs: u64,
    files: u64,
    bytes: u64,
}

fn find_walk(dir: &Path, t: &mut T) {
    let mut pat = wide(&dir.join("*"));
    let mut d = WIN32_FIND_DATAW::default();
    let Ok(h) = (unsafe { FindFirstFileExW(PCWSTR(pat.as_mut_ptr()), FindExInfoBasic, &mut d as *mut _ as *mut _, FindExSearchNameMatch, None, FIND_FIRST_EX_LARGE_FETCH) }) else {
        return;
    };
    let mut subs = Vec::new();
    loop {
        let n = &d.cFileName;
        let dot = n[0] == 46 && (n[1] == 0 || (n[1] == 46 && n[2] == 0));
        if !dot && d.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
            if d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                let len = n.iter().position(|&c| c == 0).unwrap_or(n.len());
                subs.push(dir.join(String::from_utf16_lossy(&n[..len])));
            } else {
                t.files += 1;
                t.bytes += ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
            }
        }
        if unsafe { FindNextFileW(h, &mut d) }.is_err() {
            break;
        }
    }
    let _ = unsafe { FindClose(h) };
    for s in subs {
        t.dirs += 1;
        find_walk(&s, t);
    }
}

fn info_walk(dir: &Path, t: &mut T, buf: &mut Vec<u64>) {
    let p = wide(dir);
    let Ok(h) = (unsafe {
        CreateFileW(PCWSTR(p.as_ptr()), GENERIC_READ.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None)
    }) else {
        return;
    };
    let mut subs = Vec::new();
    let mut first = true;
    loop {
        let class = if first { FileFullDirectoryRestartInfo } else { FileFullDirectoryInfo };
        first = false;
        if unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr() as *mut _, (buf.len() * 8) as u32) }.is_err() {
            break;
        }
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let e = unsafe { &*(base.add(off) as *const FILE_FULL_DIR_INFO) };
            let name = unsafe { std::slice::from_raw_parts(e.FileName.as_ptr(), e.FileNameLength as usize / 2) };
            let dot = name == [46] || name == [46, 46];
            if !dot && e.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
                if e.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                    subs.push(dir.join(String::from_utf16_lossy(name)));
                } else {
                    t.files += 1;
                    t.bytes += e.EndOfFile as u64;
                }
            }
            if e.NextEntryOffset == 0 {
                break;
            }
            off += e.NextEntryOffset as usize;
        }
    }
    let _ = unsafe { CloseHandle(h) };
    for s in subs {
        t.dirs += 1;
        info_walk(&s, t, buf);
    }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("dir"));
    for round in 0..2 {
        let t0 = Instant::now();
        let mut a = T::default();
        find_walk(&dir, &mut a);
        println!("round {round} FindFirstFileExW        {:>8.0?} {a:?}", t0.elapsed());
        for kb in [64usize, 256, 1024] {
            let mut buf = vec![0u64; kb * 1024 / 8];
            let t0 = Instant::now();
            let mut b = T::default();
            info_walk(&dir, &mut b, &mut buf);
            println!("round {round} FileFullDirInfo {kb:>4} KB {:>8.0?} {b:?}", t0.elapsed());
        }
    }
}
