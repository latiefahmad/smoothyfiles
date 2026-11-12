//! Issue #3: memory left behind by each closed window while smoothyfiles keeps running in the
//! background. Opens a window on `<folder>` through a second launch, closes it with WM_CLOSE,
//! and prints the primary process' private bytes / working set / handles after every close.
//! `cargo run --release -p rf-win --example window_leak -- <smoothyfiles.exe> <folder> <windows>`

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use rf_win::bench::{post_close, process_memory, windows_of};

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let exe = PathBuf::from(a.next().expect("smoothyfiles.exe"));
    let folder = PathBuf::from(a.next().expect("folder"));
    let count: usize = a.next().map_or(12, |s| s.parse().unwrap());
    let cfg = std::env::temp_dir().join("rf-window-leak-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(&cfg)?;
    std::fs::write(
        cfg.join("settings.toml"),
        "restore_session = false\nrun_in_background = true\nauto_update = false\nstar_asked = true\nstarts = 5\nname_index = false\ndirectlook_enabled = false\n",
    )?;
    let log = std::env::temp_dir().join("rf-window-leak.log");
    let launch = |first: bool| -> anyhow::Result<std::process::Child> {
        let mut c = Command::new(&exe);
        c.arg(&folder).env("SMOOTHYFILES_CONFIG_DIR", &cfg).env("SMOOTHYFILES_MUTE", "1");
        if first {
            c.env("RUST_LOG", "info").stderr(std::fs::File::create(&log)?);
        }
        Ok(c.spawn()?)
    };
    let mut primary = launch(true)?;
    let pid = primary.id();
    let mb = |b: u64| b as f64 / 1048576.0;
    for i in 0..=count {
        if i > 0 {
            launch(false)?.wait()?;
        }
        let t = Instant::now();
        let hwnd = loop {
            if let Some((h, _)) = windows_of(pid).into_iter().find(|(_, title)| !title.is_empty()) {
                break h;
            }
            anyhow::ensure!(t.elapsed() < Duration::from_secs(20), "no window {i}");
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(2500));
        post_close(hwnd);
        let t = Instant::now();
        while windows_of(pid).iter().any(|(h, _)| *h == hwnd) {
            anyhow::ensure!(t.elapsed() < Duration::from_secs(10), "window {i} didn't close");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(2000));
        let (ws, private, handles) = process_memory(pid).ok_or_else(|| anyhow::anyhow!("process gone"))?;
        println!("after close {i:2}: {:6.0} MB private, {:5.0} MB working set, {handles:5} handles", mb(private), mb(ws));
    }
    std::thread::sleep(Duration::from_secs(10));
    if let Some((ws, private, handles)) = process_memory(pid) {
        println!("after idle:     {:6.0} MB private, {:5.0} MB working set, {handles:5} handles", mb(private), mb(ws));
    }
    let _ = primary.kill();
    Ok(())
}
