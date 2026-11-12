//! File hashes for the properties dialog ("Hashes" tab): MD5, SHA-1, SHA-256, SHA-384, SHA-512,
//! CRC32. One streaming pass over the file computes all six, the algorithms side by side on the
//! thread pool for each chunk. Blocking: call off the UI thread.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

const BUF: usize = 1 << 20;

/// Lowercase hex digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
    pub sha384: String,
    pub sha512: String,
    pub crc32: String,
    pub size: u64,
}

/// Returned (inside `anyhow::Error`) when `cancelled` fired; use `err.downcast_ref::<Cancelled>()`.
#[derive(Debug, thiserror::Error)]
#[error("İptal edildi")]
pub struct Cancelled;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Hashes `path`. `progress(bytes_done, bytes_total)` is called after every 1 MiB chunk;
/// `cancelled` is polled at the same points.
pub fn compute(
    path: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<Hashes> {
    let mut f = File::open(path)?;
    let total = f.metadata()?.len();
    compute_reader(&mut f, total, progress, cancelled)
}

/// Same as [`compute`] for any reader (`total` is only used for progress reporting).
pub fn compute_reader(
    r: &mut dyn Read,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<Hashes> {
    let (mut md5, mut sha1, mut sha256, mut sha384, mut sha512) = (Md5::new(), Sha1::new(), Sha256::new(), Sha384::new(), Sha512::new());
    let mut crc = crc32fast::Hasher::new();
    let mut buf = vec![0u8; BUF];
    let mut done = 0u64;
    loop {
        if cancelled() {
            return Err(Cancelled.into());
        }
        // Fill the whole buffer where possible so chunk size is a stable 1 MiB.
        let mut n = 0;
        while n < BUF {
            let k = r.read(&mut buf[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        if n == 0 {
            break;
        }
        let c = &buf[..n];
        // the slow ones (the SHA-512 family, MD5) each on their own thread
        rayon::join(
            || rayon::join(|| sha512.update(c), || sha384.update(c)),
            || {
                rayon::join(
                    || md5.update(c),
                    || {
                        sha1.update(c);
                        sha256.update(c);
                        crc.update(c);
                    },
                )
            },
        );
        done += n as u64;
        progress(done, total.max(done));
        if n < BUF {
            break;
        }
    }
    Ok(Hashes {
        md5: hex(&md5.finalize()),
        sha1: hex(&sha1.finalize()),
        sha256: hex(&sha256.finalize()),
        sha384: hex(&sha384.finalize()),
        sha512: hex(&sha512.finalize()),
        crc32: format!("{:08x}", crc.finalize()),
        size: done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let mut data: &[u8] = b"abc";
        let h = compute_reader(&mut data, 3, &mut |_, _| {}, &|| false).unwrap();
        assert_eq!(h.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(h.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(h.sha384, "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7");
        assert!(h.sha512.starts_with("ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a"));
        assert_eq!(h.crc32, "352441c2");
        assert_eq!(h.size, 3);
    }

    #[test]
    fn empty_file_and_progress_and_cancel() {
        let p = std::env::temp_dir().join(format!("rf-hash-{}.bin", std::process::id()));
        std::fs::write(&p, b"").unwrap();
        let h = compute(&p, &mut |_, _| {}, &|| false).unwrap();
        assert_eq!(h.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(h.crc32, "00000000");
        // 2.5 MiB -> 3 chunks
        std::fs::write(&p, vec![7u8; BUF * 2 + BUF / 2]).unwrap();
        let mut calls = Vec::new();
        compute(&p, &mut |d, t| calls.push((d, t)), &|| false).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls.last().unwrap().0, (BUF * 2 + BUF / 2) as u64);
        let e = compute(&p, &mut |_, _| {}, &|| true).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
        std::fs::remove_file(&p).unwrap();
    }
}
