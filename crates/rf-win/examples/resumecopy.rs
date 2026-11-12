//! Resumes every interrupted job in a journal folder (the app's "Resume" without the app):
//! `resumecopy <journal folder>`. Prints what it found and how long resuming took.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rf_win::fileops::Tracker;

struct Out;
impl log::Log for Out {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        if r.target().starts_with("rf_win") {
            println!("[{}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() {
    let _ = log::set_logger(&Out).map(|_| log::set_max_level(log::LevelFilter::Info));
    let dir = PathBuf::from(std::env::args().nth(1).expect("journal folder"));
    rf_win::fastcopy::set_journal_dir(dir);
    for job in rf_win::fastcopy::interrupted() {
        println!("found: {} → {} ({} of {} files done)", job.sources[0].display(), job.dest.display(), job.done, job.files);
        // the first progress report shows where the copy picked up (0 = from the start)
        let first = std::sync::Mutex::new(None::<u32>);
        let t = Arc::new(Tracker::new(
            Box::new(move |_, done, _| {
                let mut f = first.lock().unwrap();
                if f.is_none() && done > 0 {
                    *f = Some(done);
                    println!("first progress report: {:.1} %", done as f64 / 10.0);
                }
            }),
            Arc::new(AtomicBool::new(false)),
        ));
        let start = std::time::Instant::now();
        let r = rf_win::fastcopy::resume(&job.journal, t);
        println!("resumed in {:.2} s: {r:?}", start.elapsed().as_secs_f64());
    }
}
