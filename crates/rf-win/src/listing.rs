use std::path::Path;

use rf_core::format::filetime_to_unix_ms;
use rf_core::{Entry, EntryKind, attrs};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, GetLastError,
};
use windows::Win32::Storage::FileSystem::{
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, WIN32_FIND_DATAW,
};
use windows::core::PCWSTR;

use crate::wide::{from_wide, long_path};

#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("Klasör bulunamadı")]
    NotFound,
    #[error("Erişim reddedildi")]
    AccessDenied,
    #[error("Geçersiz klasör adı")]
    InvalidName,
    #[error("Sürücü hazır değil")]
    NotReady,
    #[error("Ağ yolu bulunamadı")]
    NetworkPath,
    #[error("Listeleme hatası (kod {0})")]
    Os(u32),
    #[error("İptal edildi")]
    Cancelled,
}

/// Enumerates `dir` with the fastest Win32 path (basic info, large fetch) and streams entries
/// to `on_batch` in chunks so the UI can show the first items immediately.
///
/// `cancelled` is polled between batches; return early when the user navigated away.
pub fn list_dir(
    dir: &Path,
    batch_size: usize,
    cancelled: &dyn Fn() -> bool,
    on_batch: &mut dyn FnMut(Vec<Entry>),
) -> Result<usize, ListError> {
    let mut pattern = long_path(dir);
    pattern.pop(); // NUL
    if pattern.last() != Some(&(b'\\' as u16)) {
        pattern.push(b'\\' as u16);
    }
    pattern.push(b'*' as u16);
    pattern.push(0);

    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let handle = match handle {
        Ok(h) => h,
        Err(_) => {
            let err = unsafe { GetLastError() };
            return Err(match err {
                ERROR_FILE_NOT_FOUND => return Ok(0), // empty root
                ERROR_PATH_NOT_FOUND => ListError::NotFound,
                ERROR_ACCESS_DENIED => ListError::AccessDenied,
                // ERROR_INVALID_NAME / ERROR_DIRECTORY / ERROR_BAD_PATHNAME
                e if matches!(e.0, 123 | 267 | 161) => ListError::InvalidName,
                // ERROR_NOT_READY (empty card reader, ejected disc) / ERROR_DEV_NOT_EXIST
                e if matches!(e.0, 21 | 55) => ListError::NotReady,
                // ERROR_BAD_NETPATH / ERROR_BAD_NET_NAME / ERROR_NETWORK_UNREACHABLE / ERROR_LOGON_FAILURE…
                e if matches!(e.0, 53 | 67 | 1231 | 1222 | 64) => ListError::NetworkPath,
                e => ListError::Os(e.0),
            });
        }
    };

    let mut total = 0usize;
    let mut batch = Vec::with_capacity(batch_size.min(4096));
    // on network folders that would be a round trip per folder: skipped there
    let localize = !crate::localized::is_network(dir);
    let result = loop {
        if let Some(mut e) = to_entry(&data) {
            // Explorer's localized folder names (only read-only / system folders can have one)
            if localize && e.is_dir() && e.attrs & (attrs::READONLY | attrs::SYSTEM) != 0 {
                e.label = crate::localized::folder_label(&dir.join(&*e.name), Some(e.attrs));
            }
            batch.push(e);
            if batch.len() >= batch_size {
                total += batch.len();
                on_batch(std::mem::replace(&mut batch, Vec::with_capacity(batch_size.min(4096))));
                if cancelled() {
                    break Err(ListError::Cancelled);
                }
            }
        }
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                break Ok(());
            }
            break Err(ListError::Os(err.0));
        }
    };
    unsafe {
        let _ = FindClose(handle);
    }
    if !batch.is_empty() {
        total += batch.len();
        on_batch(batch);
    }
    result.map(|_| total)
}

/// The entry `name` in `dir` as [`list_dir`] would list it (the same source, the same fields):
/// `Ok(None)` when there is no such entry (deleted, renamed away), `Err` when it can't be told
/// (no access, the folder itself gone…). The name comes back as the disk spells it.
pub fn stat_entry(dir: &Path, name: &str) -> Result<Option<Entry>, ListError> {
    if name.is_empty() || name.contains(['*', '?', '\\', '/']) {
        return Err(ListError::InvalidName);
    }
    let mut pattern = long_path(&dir.join(name));
    if pattern.last() != Some(&0) {
        pattern.push(0);
    }
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe { FindFirstFileExW(PCWSTR(pattern.as_ptr()), FindExInfoBasic, &mut data as *mut _ as *mut _, FindExSearchNameMatch, None, Default::default()) };
    let handle = match handle {
        Ok(h) => h,
        Err(_) => {
            let err = unsafe { GetLastError() };
            return match err {
                ERROR_FILE_NOT_FOUND => Ok(None),
                ERROR_PATH_NOT_FOUND => Err(ListError::NotFound),
                ERROR_ACCESS_DENIED => Err(ListError::AccessDenied),
                e => Err(ListError::Os(e.0)),
            };
        }
    };
    unsafe {
        let _ = FindClose(handle);
    }
    let Some(mut e) = to_entry(&data) else { return Ok(None) };
    if e.is_dir() && e.attrs & (attrs::READONLY | attrs::SYSTEM) != 0 && !crate::localized::is_network(dir) {
        e.label = crate::localized::folder_label(&dir.join(&*e.name), Some(e.attrs));
    }
    Ok(Some(e))
}

pub(crate) fn to_entry(d: &WIN32_FIND_DATAW) -> Option<Entry> {
    let name = from_wide(&d.cFileName);
    if name == "." || name == ".." {
        return None;
    }
    let a = d.dwFileAttributes;
    let kind = if a & attrs::DIRECTORY != 0 { EntryKind::Dir } else { EntryKind::File };
    let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
    let ft = |f: windows::Win32::Foundation::FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Some(Entry::new(
        &name,
        kind,
        if kind == EntryKind::Dir { 0 } else { size },
        filetime_to_unix_ms(ft(d.ftLastWriteTime)),
        filetime_to_unix_ms(ft(d.ftCreationTime)),
        a,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_temp_dir() {
        let dir = std::env::temp_dir().join(format!("rf-list-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for i in 0..250 {
            std::fs::write(dir.join(format!("f{i}.txt")), b"hello").unwrap();
        }
        let mut all = Vec::new();
        let mut batches = 0;
        let n = list_dir(&dir, 100, &|| false, &mut |b| {
            batches += 1;
            all.extend(b)
        })
        .unwrap();
        assert_eq!(n, 251);
        assert_eq!(all.len(), 251);
        assert!(batches >= 3);
        assert!(all.iter().any(|e| e.is_dir() && &*e.name == "sub"));
        assert!(all.iter().any(|e| &*e.name == "f7.txt" && e.size == 5 && &*e.ext == "txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_dir() {
        let r = list_dir(Path::new(r"C:\definitely\not\here\rf"), 100, &|| false, &mut |_| {});
        assert!(matches!(r, Err(ListError::NotFound)));
    }
}
