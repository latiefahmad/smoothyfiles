//! Two builds of smoothyfiles (and File Explorer) opening the same folder, measured the same way as
//! the speed test, taking turns: `cargo run --release -p rf-win --example versions --
//! <folder> <runs> <label=exe>...`. Each smoothyfiles starts as an extra instance with its own
//! settings folder (the user's settings and running app are left alone).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rf_win::bench::{App, AppKind, measure_open};

fn main() {
    let mut a = std::env::args().skip(1);
    let folder = PathBuf::from(a.next().expect("folder"));
    let runs: usize = a.next().expect("runs").parse().unwrap();
    let cfg = std::env::temp_dir().join("rf-versions-cfg");
    let _ = std::fs::create_dir_all(&cfg);
    let _ = std::fs::write(cfg.join("settings.toml"), "restore_session = false\nrun_in_background = false\nauto_update = false\nstar_asked = true\nstarts = 1\n");
    let mut apps: Vec<(String, App)> = a
        .map(|s| {
            let (label, exe) = s.split_once('=').expect("label=exe");
            let env = vec![
                ("SMOOTHYFILES_MULTI_INSTANCE".into(), "1".into()),
                ("SMOOTHYFILES_CONFIG_DIR".into(), cfg.to_string_lossy().into_owned()),
                ("SMOOTHYFILES_MUTE".into(), "1".into()),
                ("SMOOTHYFILES_NO_INDEX".into(), "1".into()),
            ];
            (label.to_string(), App { kind: AppKind::Smoothyfiles, exe: PathBuf::from(exe), env })
        })
        .collect();
    let explorer = rf_win::bench::detect().into_iter().find(|x| x.kind == AppKind::Explorer).unwrap();
    apps.push(("File Explorer".into(), explorer));
    let cancel = AtomicBool::new(false);
    let mut results: Vec<Vec<rf_win::bench::OpenResult>> = vec![Vec::new(); apps.len()];
    for r in 0..=runs {
        for (k, (label, app)) in apps.iter().enumerate() {
            match measure_open(app, &folder, Duration::from_secs(45), &cancel) {
                Ok(m) => {
                    eprintln!("run {r} {label}: {:.0} ms, cpu {:.0} ms, private {} MB", m.settled_ms, m.cpu_ms, m.private >> 20);
                    if r > 0 {
                        results[k].push(m);
                    }
                }
                Err(e) => eprintln!("run {r} {label}: {e:#}"),
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    for ((label, _), rs) in apps.iter().zip(&mut results) {
        rs.sort_by(|a, b| a.settled_ms.total_cmp(&b.settled_ms));
        if let Some(m) = rs.get(rs.len() / 2) {
            let mut cpu: Vec<f64> = rs.iter().map(|x| x.cpu_ms).collect();
            cpu.sort_by(f64::total_cmp);
            let mut mem: Vec<u64> = rs.iter().map(|x| x.private).collect();
            mem.sort();
            println!("{label:>14}: open {:.0} ms (window {:.0}), cpu {:.0} ms, private {} MB", m.settled_ms, m.window_ms, cpu[cpu.len() / 2], mem[mem.len() / 2] >> 20);
        }
    }
}
