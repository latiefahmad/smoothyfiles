//! Extraction through rarchive, our own decoder (RAR 5, ZIP): members decode in parallel at
//! low priority and are written straight to their final files; the calling thread only
//! reports progress and relays cancellation.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};

use super::{Cancelled, ExtractStats, PasswordError, Progress, names, sanitize, timeconv};

/// rarchive's password answers as [`PasswordError`]; anything else as a message for the user.
pub(crate) fn map_err(e: rarchive::Error) -> anyhow::Error {
    match e {
        rarchive::Error::NeedPassword => PasswordError::Required.into(),
        rarchive::Error::BadPassword => PasswordError::Wrong.into(),
        e => anyhow::anyhow!(describe(&e)),
    }
}

/// What went wrong, in the user's words, with what they can do about it.
pub(crate) fn describe(e: &rarchive::Error) -> String {
    use rarchive::Error as E;
    match e {
        E::Io(x) => format!("Arşiv dosyası okunamadı: {x}"),
        E::NotArchive => "Bu dosya bir arşiv değil ya da başı bozuk.".into(),
        E::OldFormat => "Bu, RAR 1.3/1.4 biçiminde çok eski bir arşiv (1990'lar); smoothyfiles bu biçimi açmıyor. WinRAR ya da 7-Zip açabilir.".into(),
        E::Unsupported(what) => unsupported(what),
        E::Corrupt(what) => format!("Arşiv bozuk ya da yarım inmiş; yeniden indirmeyi deneyin. (Ayrıntı: {what})"),
        E::BadChecksum(name) => format!("\"{name}\" bozuk çıktı (sağlama tutmuyor). Arşiv hasarlı olabilir; yeniden indirmeyi deneyin."),
        E::NeedPassword => "Bu arşiv parola ile korunuyor.".into(),
        E::BadPassword => "Parola yanlış.".into(),
    }
}

fn unsupported(what: &str) -> String {
    let feature = match what {
        "old RAR encryption" => {
            return "Bu arşiv RAR 1.5/2.0'ın eski şifrelemesini kullanıyor; smoothyfiles bunu açmıyor. WinRAR ya da 7-Zip açabilir.".into();
        }
        "missing volume" => {
            return "Çok parçalı arşivin bir parçası eksik. Bütün parçaları (.part1.rar, .part2.rar… ya da .rar, .r00, .r01…) aynı klasöre koyup tekrar deneyin.".into();
        }
        "dictionary larger than 4 GB" => "4 GB'tan büyük sözlük (RAR 7)",
        "RAR compression method" | "unknown RAR 5 compression version" => "bilinmeyen RAR sıkıştırma yöntemi",
        "ZIP volume set" => "parçalara bölünmüş ZIP",
        "ZIP compression or encryption method" | "ZIP compression method" => "ZIP sıkıştırma ya da şifreleme yöntemi (PKWARE güçlü şifreleme olabilir)",
        "ZIP AES strength" => "ZIP AES şifreleme gücü",
        "encryption version" | "encryption method" => "şifreleme yöntemi",
        "members of unknown size" => "boyutu kayıtlı olmayan dosyalar",
        "unknown header type" => "bilinmeyen başlık türü",
        w if w.starts_with("7z") => "7z sıkıştırma yöntemi ya da başlık biçimi",
        other => other,
    };
    format!("Bu arşiv henüz desteklenmeyen bir özellik kullanıyor: {feature}.")
}

/// The member's name: for ZIP names without the UTF-8 flag, decoded with the user's legacy
/// OEM code page (what Explorer does) instead of rarchive's code page 437.
pub(crate) fn display_name(e: &rarchive::Entry) -> String {
    names::entry_name(&e.raw_name, Cow::Borrowed(&e.name)).into_owned()
}

/// Modification time: the absolute one when stored, else the DOS local time.
pub(crate) fn mtime(e: &rarchive::Entry) -> Option<std::time::SystemTime> {
    e.mtime.or_else(|| {
        let t = e.dos_time?;
        let (y, mo, d) = ((t >> 25) as u16 + 1980, ((t >> 21) & 15) as u8, ((t >> 16) & 31) as u8);
        let (h, mi, s) = (((t >> 11) & 31) as u8, ((t >> 5) & 63) as u8, ((t & 31) * 2) as u8);
        timeconv::dos_local_to_systemtime(y, mo, d, h, mi, s)
    })
}

/// State the writer threads share with the calling thread.
struct Shared<'a> {
    dest: &'a Path,
    only: &'a crate::Only,
    done: AtomicU64,
    cancel: AtomicBool,
    stats: Mutex<ExtractStats>,
    current: Mutex<String>,
    /// The first error a writer hit (its own: disk full, cancelled, …).
    error: Mutex<Option<anyhow::Error>>,
}

/// One extraction thread's output: writes its members straight to their final files.
struct DiskSink<'a> {
    sh: &'a Shared<'a>,
    open: Option<OpenFile>,
}

struct OpenFile {
    w: BufWriter<File>,
    target: PathBuf,
    mtime: Option<std::time::SystemTime>,
    written: u64,
}

impl DiskSink<'_> {
    /// Records `e` as the reason extraction stops and returns the error rarchive passes up.
    fn fail(&self, e: anyhow::Error) -> io::Error {
        self.sh.error.lock().unwrap().get_or_insert(e);
        io::Error::other("extraction stopped")
    }

    fn check_cancel(&self) -> io::Result<()> {
        if self.sh.cancel.load(Ordering::Relaxed) { Err(self.fail(Cancelled.into())) } else { Ok(()) }
    }
}

/// Raises a low-priority decoder thread to normal priority, with Windows' wake-up boost,
/// while it creates / closes a file. Those calls barely use the CPU but each waits (the
/// antivirus scans a file on close); woken at the lowest priority without the boost on a busy
/// machine, the thread queues behind everything else. 20 000 small files from a ZIP with a
/// busy IDE running: lowest + no boost 77 s, lowest + boost 48 s, normal without boost 87 s,
/// normal + boost 21 s. Decoding itself stays low.
struct IoPriority(i32, i32);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> isize;
    fn GetThreadPriority(thread: isize) -> i32;
    fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    fn GetThreadPriorityBoost(thread: isize, disabled: *mut i32) -> i32;
    fn SetThreadPriorityBoost(thread: isize, disable: i32) -> i32;
}

impl IoPriority {
    fn raise() -> Self {
        // SAFETY: plain Win32 calls on the current thread's pseudo-handle
        unsafe {
            let t = GetCurrentThread();
            let old = GetThreadPriority(t);
            let mut no_boost = 0;
            GetThreadPriorityBoost(t, &mut no_boost);
            if old < 0 {
                SetThreadPriority(t, 0);
            }
            if no_boost != 0 {
                SetThreadPriorityBoost(t, 0);
            }
            IoPriority(old, no_boost)
        }
    }
}

impl Drop for IoPriority {
    fn drop(&mut self) {
        // SAFETY: as above
        unsafe {
            if self.0 < 0 {
                SetThreadPriority(GetCurrentThread(), self.0);
            }
            if self.1 != 0 {
                SetThreadPriorityBoost(GetCurrentThread(), 1);
            }
        }
    }
}

impl rarchive::Sink for DiskSink<'_> {
    fn begin(&mut self, e: &rarchive::Entry) -> io::Result<bool> {
        self.check_cancel()?;
        let _io = IoPriority::raise();
        let name = display_name(e);
        let rel = match sanitize(&name) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(false),
            Err(x) => return Err(self.fail(x)),
        };
        if !self.sh.only.wants(&rel) {
            return Ok(false);
        }
        if e.is_link {
            self.sh.stats.lock().unwrap().skipped += 1;
            return Ok(false);
        }
        let wanted = self.sh.dest.join(&rel);
        if e.is_dir {
            std::fs::create_dir_all(&wanted)
                .map_err(|x| self.fail(anyhow::Error::new(x).context(format!("Klasör oluşturulamadı: {}", wanted.display()))))?;
            self.sh.stats.lock().unwrap().dirs += 1;
            return Ok(false);
        }
        if let Some(parent) = wanted.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|x| self.fail(anyhow::Error::new(x).context(format!("Klasör oluşturulamadı: {}", parent.display()))))?;
        }
        // Never overwrite: pick a free name; create_new closes the race window (other
        // threads may be creating files in the same folder).
        let mut target = super::unique_path(&wanted, true);
        let file = loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
                Ok(f) => break f,
                Err(x) if x.kind() == io::ErrorKind::AlreadyExists => target = super::unique_path(&wanted, true),
                Err(x) => {
                    return Err(self.fail(anyhow::Error::new(x).context(format!("Dosya oluşturulamadı: {}", target.display()))));
                }
            }
        };
        if target != wanted {
            self.sh.stats.lock().unwrap().renamed += 1;
        }
        *self.sh.current.lock().unwrap() = name;
        self.open = Some(OpenFile { w: BufWriter::with_capacity(1 << 20, file), target, mtime: mtime(e), written: 0 });
        Ok(true)
    }

    fn data(&mut self, b: &[u8]) -> io::Result<()> {
        self.check_cancel()?;
        let Some(o) = self.open.as_mut() else { return Ok(()) };
        if let Err(x) = o.w.write_all(b) {
            let target = o.target.display().to_string();
            return Err(self.fail(anyhow::Error::new(x).context(format!("Yazılamadı: {target}"))));
        }
        o.written += b.len() as u64;
        self.sh.done.fetch_add(b.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn end(&mut self, _: &rarchive::Entry) -> io::Result<()> {
        let Some(o) = self.open.take() else { return Ok(()) };
        // flush + close (+ the antivirus scan) at normal priority; `file` drops before this
        let _io = IoPriority::raise();
        let target = o.target.clone();
        let file = match o.w.into_inner() {
            Ok(f) => f,
            Err(x) => {
                let _ = std::fs::remove_file(&target);
                return Err(self.fail(anyhow::Error::new(x.into_error()).context(format!("Yazılamadı: {}", target.display()))));
            }
        };
        if let Some(t) = o.mtime {
            let _ = file.set_modified(t);
        }
        let mut st = self.sh.stats.lock().unwrap();
        st.files += 1;
        st.bytes += o.written;
        Ok(())
    }
}

impl Drop for DiskSink<'_> {
    fn drop(&mut self) {
        // a member that didn't finish (error, cancel, bad checksum): no partial file
        if let Some(o) = self.open.take() {
            drop(o.w);
            let _ = std::fs::remove_file(&o.target);
        }
    }
}

/// Extracts with rarchive: members decode in parallel (low priority, so the machine stays
/// responsive) and go straight to their files; this thread reports progress and relays
/// cancellation.
pub(crate) fn extract(
    a: rarchive::Archive,
    dest: &Path,
    only: &crate::Only,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<ExtractStats> {
    // Validate every name before writing anything.
    let mut total = 0u64;
    for e in a.entries() {
        let rel = sanitize(&display_name(e))?;
        if !e.is_dir && rel.as_deref().is_some_and(|r| only.wants(r)) {
            total += e.size;
        }
    }
    std::fs::create_dir_all(dest).with_context(|| format!("Klasör oluşturulamadı: {}", dest.display()))?;
    let sh = Shared {
        dest,
        only,
        done: AtomicU64::new(0),
        // a cancel that is already pending stops the first member
        cancel: AtomicBool::new(cancelled()),
        stats: Mutex::new(ExtractStats::default()),
        current: Mutex::new(String::new()),
        error: Mutex::new(None),
    };
    let report = |progress: &mut dyn FnMut(Progress), sh: &Shared| {
        let current = sh.current.lock().unwrap().clone();
        progress(Progress { done_bytes: sh.done.load(Ordering::Relaxed), total_bytes: total, current });
    };
    let result = std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel();
        let (a, sh) = (&a, &sh);
        s.spawn(move || {
            let r = a.extract_parallel(rarchive::threads(), || DiskSink { sh, open: None });
            let _ = tx.send(r);
        });
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(25)) {
                Ok(r) => break r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        sh.cancel.store(true, Ordering::Relaxed);
                    }
                    report(progress, sh);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(rarchive::Error::Corrupt("extraction thread ended")),
            }
        }
    });
    if let Err(e) = result {
        if let Some(own) = sh.error.lock().unwrap().take() {
            return Err(own);
        }
        return Err(map_err(e));
    }
    *sh.current.lock().unwrap() = String::new();
    report(progress, &sh);
    Ok(sh.stats.into_inner().unwrap())
}

