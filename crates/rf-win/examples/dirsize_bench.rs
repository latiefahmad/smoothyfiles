//! Size column speed: every sub-folder of a directory, one after another (old way) vs one batch.
//! `cargo run --release -p rf-win --example dirsize_bench -- <dir>`
use std::path::PathBuf;
use std::time::Instant;

use rf_win::foldersize::{SizeBatch, SizeState, folder_size, shared_cache};

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("dir"));
    let subs: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
    println!("{} sub-folders in {}", subs.len(), dir.display());
    for round in 0..2 {
        let t = Instant::now();
        let mut total = 0u64;
        for s in &subs {
            total += folder_size(s, &|| false, &mut |_| {}).map(|s| s.bytes).unwrap_or(0);
        }
        let seq = t.elapsed();
        for s in &subs {
            shared_cache().invalidate(s);
        }
        let t = Instant::now();
        let b = SizeBatch::start(subs.clone());
        let mut first = None;
        while !b.finished() {
            if first.is_none() && (0..b.len()).any(|i| matches!(b.state(i), SizeState::Done(_))) {
                first = Some(t.elapsed());
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        let batch = t.elapsed();
        let btotal: u64 = (0..b.len()).map(|i| if let SizeState::Done(s) = b.state(i) { s.bytes } else { 0 }).sum();
        let t = Instant::now();
        let b2 = SizeBatch::start(subs.clone());
        while !b2.finished() {
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        let cached = t.elapsed();
        println!(
            "round {round}: one-by-one {seq:?} | batch {batch:?} (first folder done {first:?}) | again from cache {cached:?} | {:.2} GB = {:.2} GB",
            total as f64 / 1e9,
            btotal as f64 / 1e9
        );
        for s in &subs {
            shared_cache().invalidate(s);
        }
    }
}
