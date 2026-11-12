//! Memory and speed of the name index on a real file: load (converting an older format), search,
//! save in the current format and load that again.
//! `cargo run --release -p rf-win --example index_mem -- <file.rfidx> <pattern>`

use std::path::PathBuf;
use std::time::Instant;

use rf_win::nameindex::Index;

fn private_mb() -> f64 {
    rf_win::bench::process_memory(std::process::id()).map_or(0.0, |m| m.1 as f64 / 1048576.0)
}

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let file = PathBuf::from(a.next().expect("index file"));
    let pattern = a.next().unwrap_or_else(|| "rapor".into());
    let before = private_mb();
    let t = Instant::now();
    let ix = Index::load(&file)?;
    println!(
        "loaded {} entries, {} different names in {:.2} s; index says {} MB, process +{:.0} MB",
        ix.live(),
        ix.distinct_names(),
        t.elapsed().as_secs_f32(),
        ix.memory() >> 20,
        private_mb() - before
    );
    for _ in 0..3 {
        let t = Instant::now();
        let hits = ix.search(&pattern, None, false, 10_000).unwrap_or_default();
        println!("search {pattern:?}: {} hits in {:.1} ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
    }
    let out = std::env::temp_dir().join("rf-index-mem.rfidx");
    let t = Instant::now();
    ix.save(&out)?;
    let size = std::fs::metadata(&out)?.len();
    drop(ix);
    let t2 = Instant::now();
    let back = Index::load(&out)?;
    println!(
        "saved in {:.2} s ({} MB on disk), loaded back in {:.2} s, {} entries",
        t.elapsed().as_secs_f32() - t2.elapsed().as_secs_f32(),
        size >> 20,
        t2.elapsed().as_secs_f32(),
        back.live()
    );
    let _ = std::fs::remove_file(&out);
    Ok(())
}
