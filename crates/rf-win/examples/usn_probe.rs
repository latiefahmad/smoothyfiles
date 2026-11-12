//! Can a normal (not elevated) process read a volume's change journal? Opens the drive's root
//! folder, asks for the journal, reads the latest records with the unprivileged control code
//! and resolves a parent folder id to a path.
//! `cargo run --release -p rf-win --example usn_probe -- C:\`

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType,
    GetFinalPathNameByHandleW, OPEN_EXISTING, OpenFileById, FILE_NAME_NORMALIZED,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0, USN_RECORD_V2};
use windows::core::PCWSTR;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "C:\\".into());
    let w = wide(&root);
    let h = unsafe { CreateFileW(PCWSTR(w.as_ptr()), FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None) };
    let h = match h {
        Ok(h) => h,
        Err(e) => return println!("open root: {e}"),
    };
    let mut jd = USN_JOURNAL_DATA_V0::default();
    let mut got = 0u32;
    let q = unsafe { DeviceIoControl(h, FSCTL_QUERY_USN_JOURNAL, None, 0, Some(&mut jd as *mut _ as *mut _), std::mem::size_of_val(&jd) as u32, Some(&mut got), None) };
    match q {
        Ok(()) => println!("journal id {:x}, first usn {}, next usn {}", jd.UsnJournalID, jd.FirstUsn, jd.NextUsn),
        Err(e) => return println!("query journal: {e}"),
    }
    // the last ~1 MB of records
    let start = (jd.NextUsn - (1 << 20)).max(jd.FirstUsn);
    let start = if std::env::args().nth(2).is_some() { jd.FirstUsn } else { start };
    let mut rd = READ_USN_JOURNAL_DATA_V1 { StartUsn: start, ReasonMask: 0xFFFF_FFFF, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: jd.UsnJournalID, MinMajorVersion: 2, MaxMajorVersion: 2 };
    let mut buf = vec![0u8; 1 << 16];
    let t = std::time::Instant::now();
    let (mut records, mut parents) = (0usize, std::collections::HashSet::new());
    let mut sample = None;
    loop {
        let r = unsafe {
            DeviceIoControl(h, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, Some(&rd as *const _ as *const _), std::mem::size_of_val(&rd) as u32, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, Some(&mut got), None)
        };
        if let Err(e) = r {
            println!("read journal: {e}");
            break;
        }
        if got <= 8 {
            break;
        }
        let next = i64::from_le_bytes(buf[..8].try_into().unwrap());
        let mut off = 8usize;
        while off < got as usize {
            let rec = unsafe { &*(buf.as_ptr().add(off) as *const USN_RECORD_V2) };
            if rec.RecordLength == 0 {
                break;
            }
            if rec.MajorVersion == 2 {
                records += 1;
                parents.insert(rec.ParentFileReferenceNumber);
                if sample.is_none() {
                    let name = unsafe { std::slice::from_raw_parts(buf.as_ptr().add(off + rec.FileNameOffset as usize) as *const u16, rec.FileNameLength as usize / 2) };
                    sample = Some((String::from_utf16_lossy(name), rec.ParentFileReferenceNumber));
                }
            }
            off += rec.RecordLength as usize;
        }
        rd.StartUsn = next;
        if next >= jd.NextUsn {
            break;
        }
    }
    println!("{records} records, {} folders touched, read in {:.1} ms", parents.len(), t.elapsed().as_secs_f64() * 1000.0);
    if let Some((name, parent)) = sample {
        let desc = FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: FileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: parent as i64 } };
        match unsafe { OpenFileById(h, &desc, FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, FILE_FLAG_BACKUP_SEMANTICS) } {
            Ok(ph) => {
                let mut p = [0u16; 1024];
                let n = unsafe { GetFinalPathNameByHandleW(ph, &mut p, FILE_NAME_NORMALIZED) };
                println!("sample: {name} in {}", String::from_utf16_lossy(&p[..n as usize]));
                unsafe {
                    let _ = CloseHandle(ph);
                }
            }
            Err(e) => println!("sample: {name}, parent not resolved: {e}"),
        }
    }
    unsafe {
        let _ = CloseHandle(HANDLE(h.0));
    }
}
