//! RAR creation through an installed WinRAR.
//!
//! RAR is a proprietary format: the encoder only exists inside WinRAR's console tool
//! `Rar.exe`. Without WinRAR installed there is no RAR creation ([`rar_exe`] returns `None`).
//!
//! Security note: the password is handed to `Rar.exe` as a `-hp<password>` argument (the only
//! non-interactive way it offers), so it is visible in the process list of the same user
//! session while the child runs. The child is never started through a shell.

use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_SAM_FLAGS, RegCloseKey,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::PCWSTR;

use super::{Cancelled, CompressCtx, CompressStats, Progress, collect};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a `REG_SZ` / `REG_EXPAND_SZ` value under HKLM (`None` name = the key's default value).
fn reg_string(subkey: &str, value: Option<&str>, view: REG_SAM_FLAGS) -> Option<String> {
    unsafe {
        let sub = wide(subkey);
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_QUERY_VALUE | view, &mut key)
            != ERROR_SUCCESS
        {
            return None;
        }
        let name = value.map(wide);
        let name_ptr = name.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null());
        let mut size = 0u32;
        let mut out = None;
        if RegQueryValueExW(key, name_ptr, None, None, None, Some(&mut size)) == ERROR_SUCCESS && size >= 2 {
            let mut buf = vec![0u16; size as usize / 2 + 1];
            let mut got = size;
            if RegQueryValueExW(key, name_ptr, None, None, Some(buf.as_mut_ptr().cast()), Some(&mut got))
                == ERROR_SUCCESS
            {
                let n = (got as usize / 2).min(buf.len());
                let s = String::from_utf16_lossy(&buf[..n]);
                let s = s.trim_end_matches('\0').trim().trim_matches('"').to_string();
                if !s.is_empty() {
                    out = Some(s);
                }
            }
        }
        let _ = RegCloseKey(key);
        out
    }
}

/// `Rar.exe` candidates in lookup order (existence is checked by the caller).
fn candidates() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push_exe = |exe: Option<String>| {
        if let Some(p) = exe.and_then(|e| Path::new(&e).parent().map(Path::to_path_buf)) {
            dirs.push(p);
        }
    };
    for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
        push_exe(reg_string(r"SOFTWARE\WinRAR", Some("exe64"), view));
        push_exe(reg_string(r"SOFTWARE\WinRAR", Some("exe32"), view));
        push_exe(reg_string(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\WinRAR.exe", None, view));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(Path::new(&pf).join("WinRAR"));
        }
    }
    dirs.into_iter().map(|d| d.join("Rar.exe")).collect()
}

/// Path of WinRAR's `Rar.exe`, or `None` when WinRAR is not installed (RAR creation is then
/// unavailable).
pub fn rar_exe() -> Option<PathBuf> {
    candidates().into_iter().find(|p| p.is_file())
}

/// `Rar.exe` compression method for a 0-9 UI level: 0 -> `-m0` (store) ... 9 -> `-m5`.
fn method(level: u32) -> u32 {
    level.min(9).div_ceil(2)
}

/// Arguments (without the program) of `Rar.exe a -ep1 -r -idq -y -m<n> [-hp<pw>] -- dest sources...`.
/// `-hp` encrypts file data and names. Every item is a separate argv entry.
fn rar_args(sources: &[PathBuf], dest: &Path, level: u32, password: Option<&str>) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["a", "-ep1", "-r", "-idq", "-y"].iter().map(OsString::from).collect();
    a.push(format!("-m{}", method(level)).into());
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        a.push(format!("-hp{pw}").into());
    }
    a.push("--".into());
    a.push(dest.as_os_str().to_owned());
    a.extend(sources.iter().map(|s| s.as_os_str().to_owned()));
    a
}

/// Turkish message for a `Rar.exe` exit code.
fn exit_message(code: i32) -> &'static str {
    match code {
        2 => "RAR oluşturulamadı (ölümcül hata)",
        3 => "RAR oluşturulamadı (CRC hatası)",
        4 => "RAR arşivi kilitli",
        5 => "RAR dosyasına yazılamadı (disk dolu olabilir)",
        6 => "Kaynak dosya açılamadı",
        7 => "RAR komut satırı hatası",
        8 => "RAR için yeterli bellek yok",
        9 => "RAR dosyası oluşturulamadı",
        10 => "Eklenecek dosya bulunamadı",
        11 => "RAR parolası yanlış",
        255 => "İşlem kullanıcı tarafından durduruldu",
        _ => "RAR oluşturulamadı",
    }
}

/// Creates a RAR archive with WinRAR's `Rar.exe`. `level` 0-9 maps to `-m0`..`-m5`. Never
/// overwrites `dest` ("Dosya zaten var"). `cancelled` is polled every ~100 ms; on cancel the
/// child is killed, the partial `dest` deleted and [`Cancelled`] returned. Progress is coarse
/// (start and end only). Fails with "WinRAR bulunamadı" when WinRAR is not installed.
pub fn compress_rar(
    sources: &[PathBuf],
    dest: &Path,
    level: u32,
    password: Option<&str>,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<CompressStats> {
    let exe = rar_exe().context("WinRAR bulunamadı")?;
    run_rar(&exe, sources, dest, level, password, progress, cancelled)
}

fn run_rar(
    exe: &Path,
    sources: &[PathBuf],
    dest: &Path,
    level: u32,
    password: Option<&str>,
    progress: &mut dyn FnMut(Progress),
    cancelled: &dyn Fn() -> bool,
) -> Result<CompressStats> {
    if sources.is_empty() {
        bail!("Eklenecek öğe yok");
    }
    if dest.exists() {
        bail!("Dosya zaten var");
    }
    // Used for the statistics and the progress total; also validates the sources.
    let items = collect(sources, &[dest.to_path_buf()])?;
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut ctx = CompressCtx {
        progress,
        cancelled,
        done: 0,
        total,
        last_emit: 0,
        was_cancelled: false,
        current: dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    ctx.emit(true);

    let mut child = Command::new(exe)
        .args(rar_args(sources, dest, level, password))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("Rar.exe başlatılamadı")?;
    let status = loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(dest);
            return Err(Cancelled.into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                let _ = std::fs::remove_file(dest);
                return Err(anyhow::Error::new(e).context("Rar.exe izlenemedi"));
            }
        }
    };
    let code = status.code().unwrap_or(-1);
    // 0 = success, 1 = warning (e.g. a locked file was skipped; the archive is still valid)
    if code != 0 && code != 1 {
        let _ = std::fs::remove_file(dest);
        bail!("{} (kod {code})", exit_message(code));
    }
    ctx.done = total;
    ctx.emit(true);
    Ok(CompressStats {
        path: dest.to_path_buf(),
        files: items.iter().filter(|i| !i.is_dir).count() as u64,
        dirs: items.iter().filter(|i| i.is_dir).count() as u64,
        bytes: total,
        archive_bytes: std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_are_separate_items() {
        let a = rar_args(&[PathBuf::from(r"C:\a b\x.txt"), PathBuf::from(r"C:\d")], Path::new(r"C:\o\o.rar"), 5, Some("p w"));
        let s: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert_eq!(
            s,
            ["a", "-ep1", "-r", "-idq", "-y", "-m3", "-hpp w", "--", r"C:\o\o.rar", r"C:\a b\x.txt", r"C:\d"]
        );
        let s: Vec<String> =
            rar_args(&[PathBuf::from("x")], Path::new("o.rar"), 0, None).iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(s.contains(&"-m0".to_string()) && !s.iter().any(|x| x.starts_with("-hp")));
        assert_eq!(method(9), 5);
        assert_eq!(method(99), 5);
        assert_eq!(method(1), 1);
        assert_eq!(method(0), 0);
    }

    #[test]
    fn rar_exe_is_graceful() {
        // None on machines without WinRAR; must never panic. When found, it must exist.
        if let Some(p) = rar_exe() {
            assert!(p.is_file());
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rf-rar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn existing_dest_is_refused() {
        let d = scratch("exists");
        let dest = d.join("x.rar");
        std::fs::write(&dest, b"x").unwrap();
        let src = d.join("s.txt");
        std::fs::write(&src, b"s").unwrap();
        let e = run_rar(Path::new("nonexistent-rar.exe"), &[src], &dest, 3, None, &mut |_| {}, &|| false).unwrap_err();
        assert!(format!("{e:#}").contains("Dosya zaten var"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_exe_gives_error_and_no_output() {
        let d = scratch("noexe");
        let src = d.join("s.txt");
        std::fs::write(&src, b"s").unwrap();
        let dest = d.join("y.rar");
        assert!(run_rar(Path::new("nonexistent-rar.exe"), &[src], &dest, 3, None, &mut |_| {}, &|| false).is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Needs a real WinRAR.
    #[test]
    #[ignore]
    fn real_rar_roundtrip() {
        let Some(_) = rar_exe() else { return };
        let d = scratch("real");
        std::fs::create_dir_all(d.join("p")).unwrap();
        std::fs::write(d.join("p/a.txt"), b"merhaba").unwrap();
        let dest = d.join("o.rar");
        let st = compress_rar(&[d.join("p")], &dest, 5, Some("pw"), &mut |_| {}, &|| false).unwrap();
        assert_eq!(st.files, 1);
        let l = crate::list_with(&dest, &crate::Options { password: Some("pw".into()), ..Default::default() }).unwrap();
        assert!(l.iter().any(|e| e.path.ends_with("a.txt")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
