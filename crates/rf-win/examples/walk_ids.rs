//! Does listing with file ids (`GetFileInformationByHandleEx`, needed for the change journal)
//! make the index walk slower than `FindFirstFileEx`? Alternates the two, wall time and CPU.
//! `cargo run --release -p rf-win --example walk_ids -- <folder> <rounds>`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rf_win::nameindex::{Index, bench_plain_listing};

fn cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 10_000.0;
    t(k) + t(u)
}

fn main() {
    let mut a = std::env::args().skip(1);
    let root = PathBuf::from(a.next().expect("folder"));
    let rounds: usize = a.next().map_or(3, |s| s.parse().unwrap());
    // warm the file cache once
    bench_plain_listing(true);
    let _ = Index::build(&root, &AtomicBool::new(false));
    for r in 0..rounds {
        for plain in [true, false] {
            bench_plain_listing(plain);
            let (t, c) = (Instant::now(), cpu_ms());
            let ix = Index::build(&root, &AtomicBool::new(false));
            println!(
                "round {r} {:<22} {:6.2} s wall, {:6.1} s CPU, {} entries",
                if plain { "FindFirstFileEx" } else { "with file ids" },
                t.elapsed().as_secs_f64(),
                (cpu_ms() - c) / 1000.0,
                ix.live()
            );
        }
    }
}
