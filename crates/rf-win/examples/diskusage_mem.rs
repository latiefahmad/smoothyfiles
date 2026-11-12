//! Walks a folder with the disk usage engine and reports time, entries and memory:
//! `cargo run --profile fast -p rf-win --example diskusage_mem -- C:\Windows`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rf_win::diskusage::{DiskTree, Progress};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

fn process_mb() -> (f64, f64) {
    let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb);
    }
    (m.PrivateUsage as f64 / 1_048_576.0, m.PeakWorkingSetSize as f64 / 1_048_576.0)
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| r"C:\Windows".into()));
    let before = process_mb().0;
    let progress = Progress::default();
    let t0 = Instant::now();
    let tree = DiskTree::build(&root, &AtomicBool::new(false), &progress);
    let secs = t0.elapsed().as_secs_f64();
    let (private, peak) = process_mb();
    println!("{}: {} entries, {:.1} GB, {} files, {} folders unreadable", root.display(), tree.len(), tree.size(0) as f64 / 1e9, tree.files(0), tree.skipped());
    println!("walk {secs:.1} s; tree arrays + names {:.1} MB ({:.0} bytes per entry)", tree.memory() as f64 / 1_048_576.0, tree.memory() as f64 / tree.len().max(1) as f64);
    println!("process private memory {before:.0} -> {private:.0} MB with the tree held, peak working set {peak:.0} MB (build included)");
}
