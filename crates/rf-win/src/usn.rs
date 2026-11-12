//! A volume's change journal (USN journal), read without administrator rights: which folders
//! changed since a point in time. The search index uses it at start instead of walking whole
//! drives again (seconds of work on every core) to catch what changed while the app was closed.
//!
//! `FSCTL_QUERY_USN_JOURNAL` works on any handle on the volume; the records are read with
//! `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (Windows 10 1709+), which leaves the names out but
//! keeps the parent folder's file id: that is all we need, the folders are listed again.
//! A read has to start at a record boundary: the journal's first USN or a `next` we saved.

use std::collections::HashSet;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0, USN_RECORD_V2};
use windows::core::PCWSTR;

use crate::wide::wide;

/// Where a volume's journal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Journal {
    /// Changes when the journal is deleted and created again: older positions mean nothing.
    pub id: u64,
    /// The oldest record still kept (older ones were dropped as the journal wrapped).
    pub first: i64,
    /// Where the next record will go.
    pub next: i64,
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A handle on the volume holding `path` (the folder itself, opened for attributes only).
fn open(path: &Path) -> Option<Handle> {
    let w = wide(path);
    unsafe { CreateFileW(PCWSTR(w.as_ptr()), FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None) }.ok().map(Handle)
}

/// The journal of the volume holding `path`; `None` when it has none (FAT, exFAT, network,
/// journal turned off).
pub fn query(path: &Path) -> Option<Journal> {
    let h = open(path)?;
    let mut jd = USN_JOURNAL_DATA_V0::default();
    let mut got = 0u32;
    unsafe { DeviceIoControl(h.0, FSCTL_QUERY_USN_JOURNAL, None, 0, Some(&mut jd as *mut _ as *mut _), std::mem::size_of_val(&jd) as u32, Some(&mut got), None) }.ok()?;
    Some(Journal { id: jd.UsnJournalID, first: jd.FirstUsn, next: jd.NextUsn })
}

/// Why changes since a saved point can't be read.
#[derive(Debug, PartialEq, Eq)]
pub enum Gap {
    /// Another journal now, or ours dropped records we hadn't seen: walk again.
    Lost,
    /// No journal or the read failed.
    Unreadable,
}

/// File ids of the folders that had entries created, deleted, renamed or changed since `from`
/// (a `next` saved earlier with journal `id`), and the journal's `next` after reading.
/// Bounded: more than `max` folders means "too much changed", reported as [`Gap::Lost`] (a walk
/// is then cheaper than listing them one by one).
pub fn changed_folders(path: &Path, id: u64, from: i64, max: usize) -> Result<(HashSet<u64>, i64), Gap> {
    let h = open(path).ok_or(Gap::Unreadable)?;
    let j = {
        let mut jd = USN_JOURNAL_DATA_V0::default();
        let mut got = 0u32;
        unsafe { DeviceIoControl(h.0, FSCTL_QUERY_USN_JOURNAL, None, 0, Some(&mut jd as *mut _ as *mut _), std::mem::size_of_val(&jd) as u32, Some(&mut got), None) }.map_err(|_| Gap::Unreadable)?;
        Journal { id: jd.UsnJournalID, first: jd.FirstUsn, next: jd.NextUsn }
    };
    if j.id != id || from < j.first || from > j.next {
        return Err(Gap::Lost);
    }
    let mut req = READ_USN_JOURNAL_DATA_V1 { StartUsn: from, ReasonMask: u32::MAX, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: id, MinMajorVersion: 2, MaxMajorVersion: 2 };
    let mut buf = vec![0u8; 1 << 16];
    let mut folders = HashSet::new();
    let mut at = from;
    while at < j.next {
        let mut got = 0u32;
        let r = unsafe {
            DeviceIoControl(h.0, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, Some(&req as *const _ as *const _), std::mem::size_of_val(&req) as u32, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, Some(&mut got), None)
        };
        if r.is_err() {
            // records dropped while we read, or the journal was reset
            return Err(match query(path) {
                Some(now) if now.id == id && at >= now.first => Gap::Unreadable,
                _ => Gap::Lost,
            });
        }
        if got < 8 {
            break;
        }
        let next = i64::from_le_bytes(buf[..8].try_into().unwrap());
        let mut off = 8usize;
        while off + std::mem::size_of::<USN_RECORD_V2>() <= got as usize {
            // records are 8-byte aligned in the buffer; read the header without assuming more
            let rec: USN_RECORD_V2 = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const USN_RECORD_V2) };
            if rec.RecordLength == 0 {
                break;
            }
            if rec.MajorVersion == 2 {
                folders.insert(rec.ParentFileReferenceNumber);
                if folders.len() > max {
                    return Err(Gap::Lost);
                }
            }
            off += rec.RecordLength as usize;
        }
        if next <= at {
            break;
        }
        at = next;
        req.StartUsn = next;
    }
    Ok((folders, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_file_shows_up_as_a_changed_folder() {
        let dir = std::env::temp_dir().join(format!("rf-usn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(j) = query(&dir) else {
            eprintln!("no change journal on this volume, skipped");
            return;
        };
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let (folders, next) = changed_folders(&dir, j.id, j.next, 100_000).unwrap();
        assert!(next >= j.next);
        assert!(folders.contains(&crate::usn::file_id(&dir).unwrap()), "the temp folder is among the changed ones");
        // a position from another journal is refused
        assert_eq!(changed_folders(&dir, j.id ^ 1, j.next, 10).unwrap_err(), Gap::Lost);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The file id (as the journal names parents) of a file or folder.
pub fn file_id(path: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let h = open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(h.0, &mut info) }.ok()?;
    Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}
