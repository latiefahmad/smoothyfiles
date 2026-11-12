//! Copy engine benchmark: builds a test tree once (20 000 small files in 200 folders, 20 files of
//! 50 MB), then times `fastcopy::transfer` (Copy) into a fresh folder.
//!
//!   cargo run --release -p rf-win --example copybench [work-dir] [runs]
//!
//! The work dir defaults to %TEMP%\rf-copybench. Nothing outside it is touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rf_win::fileops::{Tracker, Transfer};

fn build(src: &std::path::Path) {
    if src.join("done.marker").exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(src);
    let t = Instant::now();
    for d in 0..200 {
        let dir = src.join(format!("klasör {d:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            let body: Vec<u8> = (0..4096u32).map(|i| ((i + f * 7 + d) % 251) as u8).collect();
            std::fs::write(dir.join(format!("dosya {f:03}.txt")), body).unwrap();
        }
    }
    let big = src.join("büyük");
    std::fs::create_dir_all(&big).unwrap();
    let chunk: Vec<u8> = (0..(1u32 << 20)).map(|i| (i % 253) as u8).collect();
    for b in 0..20 {
        use std::io::Write;
        let mut f = std::fs::File::create(big.join(format!("video {b:02}.bin"))).unwrap();
        for _ in 0..50 {
            f.write_all(&chunk).unwrap();
        }
    }
    std::fs::write(src.join("done.marker"), b"").unwrap();
    println!("tree built in {:.1} s", t.elapsed().as_secs_f32());
}

fn main() {
    // BENCH_JOURNAL=1: journal the jobs (as the app does) to measure what it costs
    if std::env::var_os("BENCH_JOURNAL").is_some() {
        rf_win::fastcopy::set_journal_dir(std::env::temp_dir().join("rf-bench-journals"));
    }
    let mut args = std::env::args().skip(1);
    let work = args.next().map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("rf-copybench"));
    let runs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let src = work.join("kaynak");
    build(&src);
    let only = std::env::var("BENCH_ONLY").unwrap_or_default();
    let tops: Vec<PathBuf> = std::fs::read_dir(&src)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| !p.ends_with("done.marker"))
        .filter(|p| match only.as_str() {
            "small" => !p.ends_with("büyük"),
            "big" => p.ends_with("büyük"),
            _ => true,
        })
        .collect();
    for run in 0..runs {
        let dst = work.join(format!("hedef-{run}"));
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(&dst).unwrap();
        let tracker = Arc::new(Tracker::new(Box::new(|_, _, _| {}), Arc::new(AtomicBool::new(false))));
        let t = Instant::now();
        let ok = rf_win::fastcopy::transfer(Transfer::Copy, &tops, &dst, tracker).unwrap();
        let secs = t.elapsed().as_secs_f32();
        println!("run {run}: {secs:.2} s (ok={ok}) — {:.0} MB/s", (20.0 * 50.0 + 20000.0 * 4096.0 / 1048576.0) / secs);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
