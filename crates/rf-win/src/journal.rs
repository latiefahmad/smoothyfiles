//! A crash-safe, append-only record file: one record per line, `crc32<TAB>fields…`, where the
//! fields are separated by tabs (Windows paths can't contain tabs or line breaks). A line whose
//! checksum doesn't match, and everything after it, is ignored on reading, so a record torn by a
//! crash or a power cut is dropped instead of trusted.
//!
//! Durability: [`Writer::sync`] flushes to the disk (`FlushFileBuffers`). Callers sync what must
//! not be lost (the plan of a copy, before the first byte is copied) and batch the rest.
//!
//! The file is opened without write sharing, so another process can tell a running job (open)
//! from an interrupted one (the writer died): [`in_use`].

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

const FILE_SHARE_READ: u32 = 1;

pub struct Writer {
    out: BufWriter<File>,
}

fn line(fields: &[&str]) -> String {
    let payload = fields.join("\t");
    format!("{:08x}\t{payload}\n", crc32fast::hash(payload.as_bytes()))
}

impl Writer {
    /// A new journal file (fails if it exists).
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let f = OpenOptions::new().write(true).create_new(true).share_mode(FILE_SHARE_READ).open(path)?;
        Ok(Self { out: BufWriter::with_capacity(64 << 10, f) })
    }

    /// An existing journal, appended to (resuming a job).
    pub fn append(path: &Path) -> std::io::Result<Self> {
        let f = OpenOptions::new().append(true).share_mode(FILE_SHARE_READ).open(path)?;
        Ok(Self { out: BufWriter::with_capacity(64 << 10, f) })
    }

    pub fn record(&mut self, fields: &[&str]) -> std::io::Result<()> {
        self.out.write_all(line(fields).as_bytes())
    }

    /// Everything written so far is on the disk when this returns.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_data()
    }
}

/// The intact records of a journal, in order: reading stops at the first line that is torn or
/// fails its checksum.
pub fn read(path: &Path) -> std::io::Result<Vec<Vec<String>>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let mut out = Vec::new();
    let mut rest = &bytes[..];
    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let (ln, tail) = rest.split_at(nl);
        rest = &tail[1..];
        let Ok(s) = std::str::from_utf8(ln) else { break };
        let Some((crc, payload)) = s.split_once('\t') else { break };
        if u32::from_str_radix(crc, 16).ok() != Some(crc32fast::hash(payload.as_bytes())) {
            break;
        }
        out.push(payload.split('\t').map(str::to_string).collect());
    }
    Ok(out)
}

/// Replaces the journal with exactly `records` (dropping a corrupt tail before more is appended)
/// and returns a writer that appends to it. The new file is synced before it takes the old one's
/// place, so a crash here leaves either the old journal or the new one, never neither.
pub fn rewrite(path: &Path, records: &[Vec<String>]) -> std::io::Result<Writer> {
    let tmp = path.with_extension("jrn-new");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut w = Writer::create(&tmp)?;
        for r in records {
            w.record(&r.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        w.sync()?;
    }
    std::fs::rename(&tmp, path)?;
    Writer::append(path)
}

/// True while a writer has the journal open (its job is running, maybe in another process).
pub fn in_use(path: &Path) -> bool {
    OpenOptions::new().append(true).share_mode(FILE_SHARE_READ).open(path).is_err() && path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("rf-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    #[test]
    fn torn_and_corrupt_tails_are_dropped() {
        let p = tmp("torn");
        {
            let mut w = Writer::create(&p).unwrap();
            w.record(&["B", "c", r"C:\a b\ç.txt"]).unwrap();
            w.record(&["F", "0"]).unwrap();
            w.sync().unwrap();
            assert!(in_use(&p), "an open writer marks the job as running");
        }
        assert!(!in_use(&p));
        // a torn record (no newline) and garbage after a crash
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(line(&["F", "1"]).as_bytes()).unwrap();
        f.write_all(b"deadbeef\tF\t2\n").unwrap();
        f.write_all(line(&["F", "3"]).as_bytes()).unwrap();
        f.write_all(b"0123").unwrap();
        drop(f);
        let r = read(&p).unwrap();
        assert_eq!(r, vec![vec!["B".to_string(), "c".into(), r"C:\a b\ç.txt".into()], vec!["F".into(), "0".into()], vec!["F".into(), "1".into()]]);
        // appending after a reopen keeps the earlier records
        let mut w = Writer::append(&p).unwrap();
        w.record(&["F", "9"]).unwrap();
        w.sync().unwrap();
        drop(w);
        let _ = std::fs::remove_file(&p);
    }
}
