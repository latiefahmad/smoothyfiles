//! Our copy engine against Windows' (IFileOperation) on the same sources, alternating which goes
//! first each round: `cargo run --release -p rf-win --example copy_vs -- <dest-dir> <rounds> <source>...`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rf_win::fileops::{Tracker, Transfer};

fn main() {
    let mut a = std::env::args().skip(1);
    let dest = PathBuf::from(a.next().expect("dest dir"));
    let rounds: usize = a.next().expect("rounds").parse().unwrap();
    let sources: Vec<PathBuf> = a.map(PathBuf::from).collect();
    // BENCH_JOURNAL=1: journal the jobs as the app does (big files then copy restartably)
    if std::env::var_os("BENCH_JOURNAL").is_some() {
        rf_win::fastcopy::set_journal_dir(std::env::temp_dir().join("rf-copyvs-journals"));
    }
    let (mut ours, mut win) = (Vec::new(), Vec::new());
    for r in 0..rounds {
        for which in if r % 2 == 0 { [0, 1] } else { [1, 0] } {
            let d = dest.join(format!("r{r}-{which}"));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            let t = Instant::now();
            if which == 0 {
                let tracker = Arc::new(Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false))));
                rf_win::fastcopy::transfer(Transfer::Copy, &sources, &d, tracker).unwrap();
            } else {
                rf_win::bench::shell_copy_quiet(&sources, &d).unwrap();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            println!("round {r} {}: {ms:.0} ms", if which == 0 { "ours   " } else { "windows" });
            [&mut ours, &mut win][which].push(ms);
            let _ = std::fs::remove_dir_all(&d);
        }
    }
    ours.sort_by(f64::total_cmp);
    win.sort_by(f64::total_cmp);
    println!("median ours {:.0} ms, windows {:.0} ms", ours[ours.len() / 2], win[win.len() / 2]);
}
