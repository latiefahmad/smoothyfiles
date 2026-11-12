//! Searching subfolders, smoothyfiles against File Explorer, the way people do it: both already
//! running (Explorer's shell; a smoothyfiles with its search index ready), a search link sent to
//! each, the time until the results on screen stop changing for a second.
//! `cargo run --release -p rf-win --example search_vs -- <smoothyfiles.exe> <folder> <query> <rounds>`

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use rf_win::bench::{AppKind, measure_change, measure_launch, place_on_top, windows_of};

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let exe = PathBuf::from(a.next().expect("smoothyfiles.exe"));
    let root = PathBuf::from(a.next().expect("folder"));
    let query = a.next().expect("query");
    let rounds: usize = a.next().map_or(3, |s| s.parse().unwrap());
    let enc = |s: &str| s.replace('%', "%25").replace('&', "%26").replace('\\', "%5C").replace(':', "%3A");
    let link = format!("search-ms:query={}&crumb=location:{}", enc(&query), enc(&root.to_string_lossy()));
    let quiet = Duration::from_millis(1000);
    let timeout = Duration::from_secs(180);

    // a smoothyfiles of its own (settings folder, instance group), with the index of `root` ready
    let cfg = std::env::temp_dir().join("rf-search-vs-cfg");
    let _ = std::fs::create_dir_all(&cfg);
    std::fs::write(cfg.join("settings.toml"), "restore_session = false\nrun_in_background = false\nauto_update = false\nstar_asked = true\nstarts = 1\ndirectlook_enabled = false\n")?;
    let log = std::env::temp_dir().join("rf-search-vs.log");
    let env = |c: &mut Command| {
        c.env("SMOOTHYFILES_CONFIG_DIR", &cfg).env("SMOOTHYFILES_INDEX_ROOTS", &root).env("SMOOTHYFILES_MUTE", "1").env("RUST_LOG", "info");
    };
    let mut c = Command::new(&exe);
    c.arg(&root).stderr(std::fs::File::create(&log)?);
    env(&mut c);
    let mut app = c.spawn()?;
    let t = Instant::now();
    let hwnd = loop {
        if let Some((h, _)) = windows_of(app.id()).into_iter().next() {
            break h;
        }
        anyhow::ensure!(t.elapsed() < Duration::from_secs(20), "no smoothyfiles window");
        std::thread::sleep(Duration::from_millis(100));
    };
    place_on_top(hwnd);
    eprintln!("waiting for the index…");
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().any(|l| l.contains("indexer:") && l.contains("walked")) {
            break;
        }
        anyhow::ensure!(t.elapsed() < Duration::from_secs(300), "the index didn't get ready");
        std::thread::sleep(Duration::from_millis(500));
    }
    std::thread::sleep(Duration::from_secs(2));
    eprintln!("index ready after {:.1} s", t.elapsed().as_secs_f32());

    let explorer = rf_win::bench::detect().into_iter().find(|x| x.kind == AppKind::Explorer).unwrap();
    let cancel = AtomicBool::new(false);
    let (mut ours, mut theirs) = (Vec::new(), Vec::new());
    for r in 0..=rounds {
        let send = || -> anyhow::Result<()> {
            let mut c = Command::new(&exe);
            use std::os::windows::process::CommandExt;
            c.raw_arg(format!("\"{link}\""));
            env(&mut c);
            c.status()?;
            Ok(())
        };
        match measure_change(hwnd, send, quiet, timeout) {
            Ok((ms, ch)) => {
                eprintln!("round {r} smoothyfiles: {ms:.0} ms ({} picture changes)", ch.len());
                if r > 0 {
                    ours.push(ms);
                }
            }
            Err(e) => eprintln!("round {r} smoothyfiles: {e:#}"),
        }
        std::thread::sleep(Duration::from_millis(800));
        match measure_launch(&explorer, std::ffi::OsStr::new(&link), &query, quiet, timeout, &cancel) {
            Ok(m) => {
                eprintln!("round {r} Explorer: {:.0} ms (window {:.0} ms){}", m.settled_ms, m.window_ms, if m.timed_out { " TIMED OUT" } else { "" });
                if r > 0 {
                    theirs.push(m.settled_ms);
                }
            }
            Err(e) => eprintln!("round {r} Explorer: {e:#}"),
        }
        std::thread::sleep(Duration::from_millis(800));
    }
    let _ = app.kill();
    let med = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
    };
    let index_line = std::fs::read_to_string(&log).unwrap_or_default().lines().filter(|l| l.contains("search: the index answered")).last().map(str::to_string);
    println!("smoothyfiles: {:.0} ms   File Explorer: {:.0} ms   (medians of {rounds})", med(ours), med(theirs));
    if let Some(l) = index_line {
        println!("{l}");
    }
    Ok(())
}
