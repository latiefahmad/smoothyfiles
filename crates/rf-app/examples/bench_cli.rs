//! The Settings speed test without the UI: `cargo run --release -p rf-app --example bench_cli
//! -- [quick]`. Needs target/release/smoothyfiles.exe built. Prints the results as JSON.

use std::sync::atomic::AtomicBool;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let quick = std::env::args().any(|a| a == "quick");
    let mut me = rf_app::bench::smoothyfiles_app();
    me.exe = std::env::current_exe()?.parent().unwrap().parent().unwrap().join("smoothyfiles.exe");
    let mut apps = vec![me];
    apps.extend(rf_win::bench::detect());
    let plan = rf_app::bench::Plan {
        apps,
        open: if quick { vec![rf_app::bench::OpenTest::Files10k] } else { rf_app::bench::OpenTest::ALL.to_vec() },
        copy: !quick,
        engines: !quick,
    };
    let cancel = AtomicBool::new(false);
    let r = rf_app::bench::run(&plan, &|p| eprintln!("{p:?}"), &cancel)?;
    for o in &r.open {
        for a in &o.apps {
            match (a.median(), &a.error) {
                (Some(m), _) => eprintln!(
                    "{:?} {:>13}: {:>7.0} ms (window {:>5.0}) cpu {:>6.0} ms  ram {:>4} MB peak {:>4} MB private {:>4} MB{}{}",
                    o.test,
                    a.app.name(),
                    m.settled_ms,
                    m.window_ms,
                    m.cpu_ms,
                    m.ram >> 20,
                    m.peak_ram >> 20,
                    m.private >> 20,
                    if m.shared { " (shared process)" } else { "" },
                    if m.timed_out { " TIMED OUT" } else { "" }
                ),
                (None, Some(e)) => eprintln!("{:?} {:>13}: error {e}", o.test, a.app.name()),
                _ => {}
            }
        }
    }
    eprintln!("{:#?}", r.engines);
    eprintln!("{:?}", r.machine);
    Ok(())
}
