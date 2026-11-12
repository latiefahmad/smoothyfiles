//! RAR (read-only), entirely through our own decoder, rarchive: RAR 1.5-5, encrypted or not,
//! volume sets (`x.part1.rar`, `x.rar` + `x.r00`…; any part opens the set). Members decode in
//! parallel at low priority and go straight into the destination files.
//!
//! What rarchive can't do (RAR 1.3/1.4, the old RAR 1.5/2.0 encryption, a missing part of a set,
//! RAR 7's dictionaries over 4 GB) is refused up front with a message that says why and what to
//! do, before anything is written.

use std::path::Path;

use anyhow::{Result, anyhow};

use super::{ArchiveEntry, ExtractStats, Options, PasswordError, Progress};

/// Opens the archive (any volume of a set), with the password when one is given.
fn open(path: &Path, opts: &Options) -> Result<rarchive::Archive> {
    rarchive::Archive::open_with_password(path, opts.pw()).map_err(crate::own::map_err)
}

/// The archive, when every member can be extracted; otherwise the reason, for the user.
fn extractable(path: &Path, opts: &Options) -> Result<rarchive::Archive> {
    let a = open(path, opts)?;
    if let Some((e, why)) = a.unsupported() {
        return Err(anyhow!("{} (\"{}\")", crate::own::describe(&why), crate::own::display_name(e)));
    }
    Ok(a)
}

/// Verifies the password up front (wrong ones are refused before anything is written).
pub fn check_password(path: &Path, opts: &Options) -> Result<()> {
    extractable(path, opts)?.verify_password().map_err(crate::own::map_err)
}

/// The members. Works also for archives that can't be extracted (their names still show).
pub fn list(path: &Path, opts: &Options) -> Result<Vec<ArchiveEntry>> {
    let a = open(path, opts)?;
    // legacy (OEM) names and DOS-only times, which RAR 1.5-4 archives mostly have
    Ok(super::own_entries(&a)
        .into_iter()
        .map(|e| ArchiveEntry { size: if e.is_dir { 0 } else { e.size }, packed: if e.is_dir { 0 } else { e.packed }, ..e })
        .collect())
}

/// Encrypted headers (names hidden) or any encrypted member.
pub fn needs_password(path: &Path) -> Result<bool> {
    match rarchive::Archive::open(path) {
        Ok(a) => Ok(a.needs_password()),
        Err(rarchive::Error::NeedPassword) => Ok(true),
        Err(e) => Err(crate::own::map_err(e)),
    }
}

pub fn extract(
    path: &Path,
    dest: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    let a = extractable(path, opts)?;
    if a.needs_password() {
        if opts.pw().is_none() {
            return Err(PasswordError::Required.into());
        }
        a.verify_password().map_err(crate::own::map_err)?;
    }
    crate::own::extract(a, dest, &crate::Only::new(&opts.only), progress, cancelled)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Minimal RAR 5 writer (store method, optional multi-volume) so tests need no binary
    //! fixtures and no network.

    fn vint(mut n: u64) -> Vec<u8> {
        let mut o = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                o.push(b);
                return o;
            }
            o.push(b | 0x80);
        }
    }

    fn block(body: Vec<u8>, data: &[u8]) -> Vec<u8> {
        let mut sized = vint(body.len() as u64);
        sized.extend_from_slice(&body);
        let mut out = crc32fast::hash(&sized).to_le_bytes().to_vec();
        out.extend_from_slice(&sized);
        out.extend_from_slice(data);
        out
    }

    pub enum Item<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8]),
    }

    /// One volume. `number` 0 = first (or only) volume; `more` = another volume follows.
    pub fn volume(items: &[Item], number: u64, more: bool) -> Vec<u8> {
        let mut out = vec![0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];
        // main header: type 1, flags 0, archive flags
        let mut body = vint(1);
        body.extend(vint(0));
        if number == 0 && !more {
            body.extend(vint(0));
        } else if number == 0 {
            body.extend(vint(1)); // volume
        } else {
            body.extend(vint(3)); // volume + volume number present
            body.extend(vint(number));
        }
        out.extend(block(body, &[]));
        for it in items {
            let (name, data, dir) = match it {
                Item::Dir(n) => (*n, &[][..], true),
                Item::File(n, d) => (*n, *d, false),
            };
            let mut body = vint(2);
            body.extend(vint(if dir { 0 } else { 2 })); // header flags: data area follows
            if !dir {
                body.extend(vint(data.len() as u64));
            }
            body.extend(vint(if dir { 1 } else { 4 })); // file flags: directory / CRC present
            body.extend(vint(data.len() as u64)); // unpacked size
            body.extend(vint(0)); // attributes
            if !dir {
                body.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            }
            body.extend(vint(0)); // compression info: v0, store
            body.extend(vint(0)); // host os
            body.extend(vint(name.len() as u64));
            body.extend_from_slice(name.as_bytes());
            out.extend(block(body, data));
        }
        let mut body = vint(5);
        body.extend(vint(0));
        body.extend(vint(if more { 1 } else { 0 }));
        out.extend(block(body, &[]));
        out
    }
}
